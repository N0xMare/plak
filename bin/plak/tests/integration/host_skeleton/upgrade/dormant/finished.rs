//! A submitted real child compiles passively from the semantic source tree.
use super::*;

#[test]
fn host_move_dormant_preserves_a_finished_child_without_reviving_its_grant() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let release = fixture.root.join("child-release");
    let child = fixture.root.join("child.json");
    std::fs::write(
        fixture.root.join("grandchild-script.json"),
        "{\"version\":1,\"steps\":[]}",
    )
    .unwrap();
    std::fs::write(&child, serde_json::to_vec(&json!({"version":1,"repeat_last_step":true,"steps":[{
        "stream":[{"gate":{"file":release}}],"output":{"tool":{"name":"submit_result","arguments":{
            "turn_token":1,"output_json":"{\"summary\":\"finished child result\"}"
        }}},"finish":"tool_calls"
    }]})).unwrap()).unwrap();
    let mut script = spawning_script();
    script["product_child_script"] = json!(child);
    script["steps"].as_array_mut().unwrap().remove(1);
    fixture.write_script(&script);
    let mut host = start_env(&fixture, &[], &[(ENABLED, "1".into())]);
    let (session, node) = create_and_prompt(&fixture, &mut host, "spawn a result-submitting child");
    wait_idle(&fixture, &session, 1);
    std::fs::write(&release, "finish").unwrap();
    wait_idle(&fixture, &session, 2);
    stop(&mut host);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let before = plak::store::tree::load_tree(&paths, &node.tree_id)
        .unwrap()
        .unwrap();
    let child = before
        .nodes()
        .values()
        .find(|node| node.parent_agent_id.is_some())
        .unwrap()
        .clone();
    assert!(!child.active_turn);
    assert!(
        before
            .attempts()
            .values()
            .any(|attempt| attempt.agent_id == child.agent_id && attempt.output_json.is_some())
    );
    let granted = before.grants().contains_key(&child.agent_id);
    let mut host = start_env(&fixture, &[], &[(ENABLED, "1".into())]);
    let dispatched = crash::dispatches(&fixture);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(
        result.status.success(),
        "finished child refused (source grant={granted}): {result:?}"
    );
    assert_eq!(successor(&fixture, &session), session);
    assert_eq!(crash::dispatches(&fixture), dispatched);
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
    let next = target.rows().nodes().get(&child.agent_id).unwrap();
    assert_eq!(next.status, child.status);
    assert!(!next.active_turn);
    assert_eq!(
        target.rows().grants().contains_key(&child.agent_id),
        granted
    );
    stop(&mut host);
}

pub(super) fn create_and_prompt(
    fixture: &Fixture,
    host: &mut Host,
    prompt: &str,
) -> (String, NodeAddress) {
    let mut client = Client::connect(host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("root did not create")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, prompt, ANSWER);
    assert_eq!(
        client.request(4, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    (session, node)
}

pub(super) fn wait_idle(fixture: &Fixture, session: &str, turns: i64) {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let records = plak::store::session_records::SqliteSessionStore::new(paths.clone());
    let deadline = Instant::now() + WAIT;
    loop {
        let record = plak::store::sessions::load_session(&paths, session).unwrap();
        if records
            .load_views(session)
            .unwrap()
            .is_some_and(|views| views.state.open_turns.is_empty())
            && i64::try_from(record.root_turn_count).unwrap() >= turns
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "root did not finish {turns} turns"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
