//! A completed child wakes an idle hosted parent, including after handoff.

use super::*;
use plak::store::session_records::SessionRecordStore as _;
use plakous_recursive_subagents::tree::{ProductAgentTree, TreeStore as _};
use plakous_recursive_subagents_store::SqliteTreeStore;
use std::time::{Duration, Instant};

fn idle_turns(fixture: &Fixture, session: &str, expected: u64) {
    let records = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let deadline = Instant::now() + WAIT;
    loop {
        let views = records.load_views(session).unwrap().unwrap();
        let turns = views.state.counters.root_turns;
        if turns >= expected && views.state.open_turns.is_empty() {
            assert_eq!(turns, expected, "exactly one root wake");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "root did not finish turn {expected}: turns={turns}, suppressed={}",
            views.wake_suppressed(&records.load_intents(session).unwrap())
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn submitted_result(fixture: &Fixture, node: &NodeAddress) {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let trees = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let tree = ProductAgentTree::reopen(trees.load(&node.tree_id).unwrap().unwrap());
        let outputs: Vec<_> = tree
            .attempts()
            .values()
            .filter_map(|attempt| attempt.output_json.as_deref())
            .collect();
        if !outputs.is_empty() {
            assert_eq!(outputs, ["{\"summary\":\"review complete\"}"]);
            return;
        }
        assert!(Instant::now() < deadline, "child did not submit its result");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_child_result_wakes_the_idle_root_before_and_after_a_busy_handoff() {
    for restart in [false, true] {
        let Some(fixture) = fixture() else { return };
        crash::client_control::script(&fixture, true);
        // The product-child fixture includes its caller's topology tools
        // when the nested-child route is configured; no grandchild is spawned.
        std::fs::write(
            fixture.root.join("grandchild-script.json"),
            json!({"version":1,"steps":[]}).to_string(),
        )
        .unwrap();
        let child_gate = fixture.root.join("release-child");
        std::fs::write(
            fixture.root.join("child.json"),
            json!({
                "version": 1,
                "steps": [{
                    "stream": [{"text":"child working"}, {"gate":{"file":child_gate}}],
                    "output":{"tool":{"name":"submit_result","arguments":{
                        "turn_token":1,"output_json":"{\"summary\":\"review complete\"}"
                    }}}, "finish":"tool_calls"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let mut script: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
        if restart {
            script["steps"][1]["stream"] = json!([
                {"text":PREFIX}, {"gate":{"file":fixture.root.join("root-not-released")}}
            ]);
        }
        fixture.write_script(&script);
        let mut host = start_env(&fixture, &[], &[("PLAK_LOG", "info".to_owned())]);
        let mut client = Client::connect(&host);
        let Outcome::Ok(Answer::Created { session, node, .. }) =
            client.request(1, new_session(&fixture.workspace()))
        else {
            panic!("session not created")
        };
        let attachment = client.attach(2, &node);
        client.prompt(
            3,
            attachment,
            "delegate and idle",
            if restart { PREFIX } else { ANSWER },
        );
        if restart {
            stop(&mut host);
            assert_eq!(
                root_record(&fixture, &session),
                (vec![RootInterruptCause::Handoff], 0)
            );
            script["steps"] = json!([{"output":{"text":ANSWER}}]);
            fixture.write_script(&script);
            host = start_env(&fixture, &[], &[("PLAK_LOG", "info".to_owned())]);
            client = Client::connect(&host);
            client.attach(4, &node);
        }
        let before = if restart { 2 } else { 1 };
        idle_turns(&fixture, &session, before);
        assert_eq!(root_record(&fixture, &session).1, usize::from(restart));
        std::fs::write(child_gate, "continue").unwrap();
        submitted_result(&fixture, &node);
        idle_turns(&fixture, &session, before + 1);
        let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
        let trees = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
        let tree = ProductAgentTree::reopen(trees.load(&node.tree_id).unwrap().unwrap());
        assert_eq!(
            tree.messages()
                .values()
                .filter(|mail| {
                    mail.purpose == plakous_recursive_subagents::protocol::MessagePurpose::Result
                        && mail.to == tree.root_agent_id
                })
                .count(),
            1,
            "the real child's submitted result reached its parent"
        );
        drop(client);
        stop(&mut host);
    }
}
