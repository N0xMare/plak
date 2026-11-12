//! Startup takes an idle parent's tree only for a recorded child handoff.

use super::*;
use plakous_recursive_subagents::tree::{ProductAgentTree, TreeStore as _};
use plakous_recursive_subagents_store::SqliteTreeStore;

fn child_calls(fixture: &Fixture) -> usize {
    use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
    read_evidence(fixture.root.join("child-evidence.jsonl"))
        .unwrap_or_default()
        .iter()
        .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
        .count()
}

fn wait_child_calls(fixture: &Fixture, count: usize) {
    let deadline = std::time::Instant::now() + WAIT;
    while child_calls(fixture) < count {
        assert!(
            std::time::Instant::now() < deadline,
            "child never dispatched request {count}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn gate_child(fixture: &Fixture, gate: &str) {
    std::fs::write(fixture.root.join("child.json"), json!({
        "version": 1, "repeat_last_step": true,
        "steps": [{"stream": [{"text": "child working"}, {"gate": {"file": fixture.root.join(gate)}}],
                   "output": {"text": "child done"}}]
    }).to_string()).unwrap();
}

fn start_idle_parent(fixture: &Fixture) -> (Host, String, NodeAddress) {
    crash::client_control::script(fixture, true);
    gate_child(fixture, "first-gate");
    let host = start(fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("session not created")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "delegate a review", ANSWER);
    wait_child_calls(fixture, 1);
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the parent never idled"
        );
        if matches!(
            client.next(),
            HostFrame::Event {
                observation: plakous_host_protocol::SessionObservation::Status {
                    phase: plakous_host_protocol::status::HostPhase::Idle,
                    ..
                },
                ..
            }
        ) {
            break;
        }
    }
    (host, session, node)
}

fn scan_complete(host: &Host) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "startup scan did not finish"
        );
        let line = host
            .lines
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .unwrap();
        if line == "plak host: continuation scan complete" {
            return;
        }
    }
}

#[test]
fn an_idle_parent_continues_its_handed_off_child_at_startup_without_an_attach() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, node) = start_idle_parent(&fixture);
    stop(&mut host);
    assert_eq!(
        root_record(&fixture, &session),
        (vec![], 0),
        "no root handoff to discover"
    );
    gate_child(&fixture, "continuation-gate");
    let mut host = start(&fixture);
    scan_complete(&host);
    // No connection or attach has been made to the replacement host.
    wait_child_calls(&fixture, 2);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let tree = ProductAgentTree::reopen(store.load(&node.tree_id).unwrap().unwrap());
    assert_eq!(
        tree.commands()
            .keys()
            .filter(|id| id.starts_with(ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX))
            .count(),
        1
    );
    let mut client = Client::connect(&host);
    assert_eq!(
        client
            .rows(1)
            .iter()
            .find(|row| row.session == session)
            .unwrap()
            .residency,
        SessionResidency::Resident
    );
    client.attach(2, &node);
    assert_eq!(
        child_calls(&fixture),
        2,
        "attach must not duplicate the continuation"
    );
    drop(client);
    stop(&mut host);
}

#[test]
fn an_idle_parent_without_a_child_handoff_stays_dormant_at_startup() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, _node) = start_idle_parent(&fixture);
    kill(&mut host);
    assert_eq!(root_record(&fixture, &session), (vec![], 0));
    gate_child(&fixture, "not-resumed-gate");
    let mut host = start(&fixture);
    scan_complete(&host);
    let mut client = Client::connect(&host);
    assert_eq!(
        client
            .rows(1)
            .iter()
            .find(|row| row.session == session)
            .unwrap()
            .residency,
        SessionResidency::Dormant
    );
    assert_eq!(
        child_calls(&fixture),
        1,
        "a crash grants no handoff continuation"
    );
    drop(client);
    stop(&mut host);
}
