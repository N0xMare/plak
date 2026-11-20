use super::*;
use plakous_hosted::agent_tree_protocol::NodeStatus;

#[test]
fn same_key_upgrade_reopens_a_pre_placement_same_runner_tree() {
    let pair = Pair::new();
    let _destination = upgrade::HomeHost(pair.host.home());
    let mut main_shaped = spawn("local", "fork");
    main_shaped["output"]["tool"]["arguments"]
        .as_object_mut()
        .unwrap()
        .remove("runner");
    let (host, mut client, session, node, attachment) = pair.session(
        false,
        &[main_shaped, wait(), answer("same-runner copy settled")],
    );
    client.prompt(
        13,
        attachment,
        "run the original same-runner shape",
        "same-runner copy settled",
    );
    let results = tool_results(&pair.host, &session, 2);
    let agent = results[0]["agent_id"].as_str().unwrap();
    let saved = tree(&pair, &node);
    let child = saved.node(agent).unwrap();
    assert_eq!(
        child.placement.runner_id.as_deref(),
        Some(pair.launch_id.as_str())
    );
    assert!(
        child
            .path
            .starts_with(pair.host.workspace().join(".plak/worktrees"))
    );
    assert!(saved.runner_ancestors().is_empty());
    let upgraded = upgrade::run_same_key_fixture_upgrade(&pair.host, &pair.path);
    assert!(
        upgraded.status.success(),
        "{}",
        String::from_utf8_lossy(&upgraded.stderr)
    );
    let mut client = Client::connect_at(&host.socket);
    pending_stop::ready(&mut client, &pair.launch_id);
    client.attach(14, &node);
    client.attach(
        15,
        &NodeAddress {
            tree_id: node.tree_id.clone(),
            agent_id: agent.to_owned(),
        },
    );
    let reopened = tree(&pair, &node);
    assert_eq!(reopened.node(agent).unwrap().path, child.path);
    assert_eq!(
        reopened.node(agent).unwrap().placement.runner_id.as_deref(),
        Some(pair.launch_id.as_str())
    );
}

fn begin(client: &mut Client, attachment: u64) {
    assert_eq!(
        client.request(
            31,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("exercise the carried fork")),
                blobs: Vec::new(),
            },
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
}

fn replace_steps(pair: &Pair, steps: &[Value]) {
    let mut script: Value =
        serde_json::from_slice(&std::fs::read(pair.host.script()).unwrap()).unwrap();
    script["steps"] = json!(steps);
    pair.host.write_script(&script);
}

fn gate(directory: &Path, boundary: &str, state: &str) {
    write_cut_marker(&directory.join(format!("{boundary}.{state}")), b"cut");
}

fn crossed_copy(pair: &Pair, node: &NodeAddress) -> PathBuf {
    let directory = pair
        .copies
        .join(&pair.secondary_id)
        .join(&node.tree_id)
        .join(".plak/worktrees");
    let mut copies = std::fs::read_dir(directory).unwrap();
    let copy = copies.next().unwrap().unwrap().path();
    assert!(
        copies.next().is_none(),
        "at most one copy for the one spawn"
    );
    copy
}

fn recorded_tree(pair: &Pair, node: &NodeAddress) -> plakous_hosted::agent_tree::ProductAgentTree {
    use plakous_recursive_subagents_store::{SqliteTreeStore, TreeInspection};
    let paths = plakd::home::paths::PlakPaths::explicit(pair.host.home());
    let TreeInspection::Tree(stored) =
        SqliteTreeStore::inspect(&paths.subagents_database(), &node.tree_id).unwrap()
    else {
        panic!("expected an existing, published tree");
    };
    stored.rows().clone()
}

#[test]
fn host_cut_after_cross_runner_mint_before_child_publication() {
    let pair = Pair::new();
    let mut spawn = spawn("secondary", "fork");
    spawn["output"]["tool"]["arguments"]["request_key"] = json!("carry-cut");
    let (mut host, mut client, session, node, attachment) =
        pair.session(false, &[spawn.clone(), answer("child assigned")]);
    pending_stop::ready(&mut client, &pair.secondary_id);
    gate(&pair.cuts, "after-mint", "armed");
    begin(&mut client, attachment);
    wait_for(|| pair.cuts.join("after-mint.entered").is_file());
    let before = crossed_copy(&pair, &node);
    assert_eq!(
        std::fs::read_to_string(before.join("draft.txt")).unwrap(),
        "live parent\n"
    );
    let saved = recorded_tree(&pair, &node);
    assert_eq!(saved.nodes().len(), 1);
    assert!(saved.operations().values().any(|row| {
        row.tool_name == "vcs_stage"
            && serde_json::from_str::<Value>(&row.canonical_request).unwrap()["stage"] == "mint"
    }));
    drop(client);
    kill(&mut host);
    gate(&pair.cuts, "after-mint", "release");

    replace_steps(&pair, &[spawn, wait(), answer("retry settled")]);
    let mut host = pair.start();
    let mut client = Client::connect(&host);
    pending_stop::ready(&mut client, &pair.launch_id);
    pending_stop::ready(&mut client, &pair.secondary_id);
    let attachment = client.attach(32, &node);
    assert_eq!(tree(&pair, &node).nodes().len(), 1);
    client.prompt(
        33,
        attachment,
        "retry the same qualified spawn",
        "retry settled",
    );
    let results = tool_results(&pair.host, &session, 2);
    let receipt = results
        .iter()
        .find(|value| value["runner"] == pair.secondary_id)
        .unwrap();
    let saved = tree(&pair, &node);
    assert_eq!(saved.nodes().len(), 2);
    let child = saved.node(receipt["agent_id"].as_str().unwrap()).unwrap();
    assert_eq!(child.path, before);
    assert_eq!(crossed_copy(&pair, &node), before);
    assert_eq!(
        std::fs::read_to_string(before.join("remote.txt")).unwrap(),
        "placed\n"
    );
    stop(&mut host);
}

#[test]
fn host_cut_after_cross_runner_seal_apply_recovers_the_exact_pin() {
    let pair = Pair::new();
    let (mut host, mut client, session, node, attachment) = pair.session(
        false,
        &[spawn("secondary", "fork"), answer("child assigned")],
    );
    pending_stop::ready(&mut client, &pair.secondary_id);
    gate(&pair.launch_cuts, "after-bundle-apply", "armed");
    begin(&mut client, attachment);
    wait_for(|| {
        pair.launch_cuts
            .join("after-bundle-apply.entered")
            .is_file()
    });
    let before = recorded_tree(&pair, &node);
    assert!(before.candidates().is_empty());
    let child = before
        .nodes()
        .values()
        .find(|node| node.depth == 1)
        .unwrap();
    assert_eq!(child.status, NodeStatus::Sealing);
    let agent = child.agent_id.clone();
    let apply = before
        .operations()
        .values()
        .filter(|row| row.tool_name == "vcs_stage" && row.caller_agent_id == agent)
        .map(|row| serde_json::from_str::<Value>(&row.canonical_request).unwrap())
        .find(|intent| intent["stage"] == "bundle_apply" && intent["class"] == "verifiable")
        .expect("a verifiable return stage was published before apply");
    let candidate = apply["candidate_id"].as_str().unwrap();
    drop(client);
    kill(&mut host);
    gate(&pair.launch_cuts, "after-bundle-apply", "release");
    // Alter the source checkout after the cut. Recovery must not take a new
    // snapshot instead of proving the already-applied candidate.
    std::fs::write(child.path.join("remote.txt"), "after the cut\n").unwrap();
    replace_steps(
        &pair,
        &[adopt(candidate, false), answer("pinned candidate adopted")],
    );
    let mut host = pair.start();
    let mut client = Client::connect(&host);
    pending_stop::ready(&mut client, &pair.launch_id);
    pending_stop::ready(&mut client, &pair.secondary_id);
    let attachment = client.attach(34, &node);
    wait_for(|| {
        let saved = tree(&pair, &node);
        assert_ne!(
            saved.node(&agent).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        saved.candidates().values().next().is_some_and(|saved| {
            assert_eq!(saved.commit, apply["expected"]["commit"]);
            true
        })
    });
    client.prompt(
        35,
        attachment,
        "adopt the recovered pin",
        "pinned candidate adopted",
    );
    assert!(
        tool_results(&pair.host, &session, 2)
            .iter()
            .any(|value| value["files_changed"].is_number())
    );
    assert_eq!(
        std::fs::read_to_string(pair.host.workspace().join("remote.txt")).unwrap(),
        "placed\n"
    );
    stop(&mut host);
}

#[test]
fn unstarted_seal_survives_both_legs_and_reopen_with_the_child_runner_down() {
    let pair = Pair::new();
    let (mut host, mut client, _, node, attachment) = pair.session(
        false,
        &[spawn("secondary", "fork"), answer("child assigned")],
    );
    pending_stop::ready(&mut client, &pair.secondary_id);
    let child_script = pair.host.root.join("placed-child.json");
    let mut child: Value = serde_json::from_slice(&std::fs::read(&child_script).unwrap()).unwrap();
    let arguments = &mut child["steps"][0]["output"]["tool"]["arguments"];
    arguments["command"] = json!(
        "printf 'placed\\n' > remote.txt; while [ ! -f finish-seal ]; do sleep 0.02; done; rm finish-seal"
    );
    arguments["timeout"] = json!(30);
    std::fs::write(child_script, serde_json::to_vec(&child).unwrap()).unwrap();
    client.prompt(
        36,
        attachment,
        "spawn before cutting the parent",
        "child assigned",
    );
    let copy = crossed_copy(&pair, &node);
    wait_for(|| copy.join("remote.txt").is_file());
    refuse(&pair.launch_cuts);
    let mut launch = Paused::new(pair.launch.pid().unwrap());
    wait_for(|| !is_ready(&mut client, &pair.launch_id));
    std::fs::write(copy.join("finish-seal"), b"finish").unwrap();
    let agent = tree(&pair, &node)
        .nodes()
        .values()
        .find(|node| node.depth == 1)
        .unwrap()
        .agent_id
        .clone();
    wait_for(|| tree(&pair, &node).node(&agent).unwrap().status == NodeStatus::Sealing);
    assert_unstarted(&pair, &node, &agent);

    refuse(&pair.cuts);
    let mut secondary = Paused::new(pair.secondary.pid().unwrap());
    wait_for(|| !is_ready(&mut client, &pair.secondary_id));
    launch.resume();
    std::fs::remove_file(pair.launch_cuts.join("refuse")).unwrap();
    pending_stop::ready(&mut client, &pair.launch_id);
    assert_unstarted(&pair, &node, &agent);
    drop(client);
    kill(&mut host);
    replace_steps(&pair, &[answer("root still works")]);

    let mut host = pair.start();
    let mut client = Client::connect(&host);
    pending_stop::ready(&mut client, &pair.launch_id);
    let root = client.attach(37, &node);
    client.prompt(
        38,
        root,
        "prove the root is independent",
        "root still works",
    );
    assert!(!is_ready(&mut client, &pair.secondary_id));
    assert_unstarted(&pair, &node, &agent);
    secondary.resume();
    std::fs::remove_file(pair.cuts.join("refuse")).unwrap();
    pending_stop::ready(&mut client, &pair.secondary_id);
    wait_for(|| tree(&pair, &node).candidates().len() == 1);
    stop(&mut host);
}

fn refuse(cuts: &Path) {
    write_cut_marker(&cuts.join("refuse"), b"offline");
}

fn is_ready(client: &mut Client, id: &str) -> bool {
    let Outcome::Ok(Answer::Options { options }) =
        client.request(39, Op::SessionOptions { workspace: None })
    else {
        panic!("no runner options");
    };
    options
        .runners
        .iter()
        .any(|row| row.runner.runner_id == id && row.status == RunnerStatus::Ready)
}

fn assert_unstarted(pair: &Pair, node: &NodeAddress, agent: &str) {
    let saved = recorded_tree(pair, node);
    assert_eq!(saved.node(agent).unwrap().status, NodeStatus::Sealing);
    assert!(saved.candidates().is_empty());
    assert!(
        !saved
            .operations()
            .values()
            .any(|row| row.caller_agent_id == agent && row.tool_name == "vcs_stage")
    );
}

#[test]
fn a_working_child_defers_recovery_when_its_runner_is_down_at_reopen() {
    let pair = Pair::new();
    let (mut host, mut client, _, node, attachment) = pair.session(
        false,
        &[spawn("secondary", "fork"), answer("child assigned")],
    );
    pending_stop::ready(&mut client, &pair.secondary_id);
    gate(&pair.cuts, "before-answer", "armed");
    client.prompt(
        41,
        attachment,
        "start the child before the cut",
        "child assigned",
    );
    wait_for(|| pair.cuts.join("before-answer.entered").is_file());
    let saved = recorded_tree(&pair, &node);
    let child = saved.nodes().values().find(|node| node.depth == 1).unwrap();
    assert!(child.active_turn);
    let agent = child.agent_id.clone();
    refuse(&pair.cuts);
    let mut secondary = Paused::new(pair.secondary.pid().unwrap());
    drop(client);
    kill(&mut host);
    gate(&pair.cuts, "before-answer", "release");
    replace_steps(&pair, &[answer("root reopened")]);

    let mut host = pair.start();
    let mut client = Client::connect(&host);
    pending_stop::ready(&mut client, &pair.launch_id);
    let root = client.attach(42, &node);
    client.prompt(
        43,
        root,
        "open the root without the child runner",
        "root reopened",
    );
    let saved = recorded_tree(&pair, &node);
    let retained = saved.node(&agent).unwrap();
    assert!(
        retained.active_turn,
        "the offline working child has not been settled"
    );
    assert_eq!(
        retained.placement.runner_id.as_deref(),
        Some(pair.secondary_id.as_str())
    );
    assert_ne!(retained.status, NodeStatus::RecoveryRequired);
    assert!(!is_ready(&mut client, &pair.secondary_id));
    secondary.resume();
    std::fs::remove_file(pair.cuts.join("refuse")).unwrap();
    pending_stop::ready(&mut client, &pair.secondary_id);
    wait_for(|| {
        !recorded_tree(&pair, &node)
            .node(&agent)
            .unwrap()
            .active_turn
    });
    let dispatches = plakous_offline_crash_harness::evidence::read_evidence(
        pair.host.root.join("child-evidence.jsonl"),
    )
    .unwrap()
    .into_iter()
    .filter(|event| {
        event.kind == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
    })
    .count();
    assert_eq!(dispatches, 1, "readiness is not paid continuation consent");
    stop(&mut host);
}
