//! Operator controls still suppress an idle root after its handoff continuation.

use super::*;

#[test]
fn an_idle_interrupt_after_handoff_keeps_a_later_result_from_waking_the_root() {
    let Some((fixture, script)) = child_fixture(false) else {
        return;
    };
    let (mut host, mut client, session, node, attachment) =
        handed_off_root(&fixture, script, json!([{"output":{"text":ANSWER}}]));
    idle_turns(&fixture, &session, 2);
    assert_eq!(
        client.request(
            10,
            Op::Intent {
                attachment,
                intent: SessionIntent::Interrupt,
                client_id: None,
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    wait_suppressed(&fixture, &session);
    std::fs::write(fixture.root.join("release-child"), "continue").unwrap();
    submitted_result(&fixture, &node);
    wait_for_result(&fixture, &node);
    assert_parked(&fixture, &session);
    drop(client);
    stop(&mut host);
}

#[test]
fn a_stop_after_handoff_keeps_a_pending_child_result_from_waking_the_root() {
    let Some((fixture, script)) = child_fixture(false) else {
        return;
    };
    let (mut host, mut client, session, node, _) =
        handed_off_root(&fixture, script, json!([{"output":{"text":ANSWER}}]));
    idle_turns(&fixture, &session, 2);
    // A stopped child cannot submit successfully. Hold automatic waking off
    // until its result is pending, rather than race the coalescing timer.
    let config_path = fixture.home().join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    write_private(
        &config_path,
        &format!("{config}\n[turn]\nauto_continue = false\n"),
    );
    wait_decision(&fixture, &session, "host_wakes_off");
    std::fs::write(fixture.root.join("release-child"), "continue").unwrap();
    submitted_result(&fixture, &node);
    wait_for_result(&fixture, &node);
    assert_eq!(
        client.request(
            10,
            Op::StopSession {
                session: session.clone()
            }
        ),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    wait_suppressed(&fixture, &session);
    write_private(&config_path, &config);
    assert_parked(&fixture, &session);
    wait_for_result(&fixture, &node);
    drop(client);
    stop(&mut host);
}

fn wait_suppressed(fixture: &Fixture, session: &str) {
    let records = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let deadline = Instant::now() + WAIT;
    loop {
        if records
            .load_views(session)
            .unwrap()
            .unwrap()
            .wake_suppressed(&records.load_intents(session).unwrap())
        {
            return;
        }
        assert!(Instant::now() < deadline, "operator pause was not recorded");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_parked(fixture: &Fixture, session: &str) {
    wait_decision(fixture, session, "suppressed");
    idle_stays(fixture, session, 2);
}

fn wait_decision(fixture: &Fixture, session: &str, reason: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let log = std::fs::read_to_string(fixture.home().join("logs/plak.log")).unwrap();
        if log
            .lines()
            .any(|line| line.contains(session) && line.contains("parked") && line.contains(reason))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "idle wake did not park for {reason}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_result(fixture: &Fixture, node: &NodeAddress) {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let trees = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let tree = ProductAgentTree::reopen(trees.load(&node.tree_id).unwrap().unwrap());
        if tree.unread_mail(&tree.root_agent_id).any(|mail| {
            mail.purpose == plakous_recursive_subagents::protocol::MessagePurpose::Result
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the submitted result did not stay unread"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
