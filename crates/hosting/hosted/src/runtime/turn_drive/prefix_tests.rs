//! Real runtime commits ahead of a deliberately delayed hosted observer.

#![allow(clippy::unwrap_used, reason = "test-owned commits and rendezvous")]
use crate::runtime::test_kit::BlockOn as _;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use plakous::sessions::feed::FeedFact;
use plakous_offline_crash_harness::gate::GateHandle;

use super::{HostBoundary, HostWorkflow};
use crate::runtime::projection::live::LiveTarget;
use crate::runtime::recover_tests::{script, scripted_host_with, turn_completed, wait_for};
use crate::runtime::test_support::send_attached_action;
use crate::surface_protocol::UserAction;

struct RoundProbe {
    passes: Vec<(u64, u64)>,
    rounds: usize,
    last_stream: Arc<GateHandle>,
}

fn probes() -> &'static Mutex<BTreeMap<String, RoundProbe>> {
    static PROBES: OnceLock<Mutex<BTreeMap<String, RoundProbe>>> = OnceLock::new();
    PROBES.get_or_init(Mutex::default)
}

pub(super) fn after_round_pass(target: &LiveTarget, prefix: u64) {
    let mut probes = probes().lock().unwrap();
    let Some(probe) = probes.get_mut(&target.product_id) else {
        return;
    };
    let cursor = crate::topology_runtime::lookup(&target.tree_id)
        .unwrap()
        .read(|tree| tree.feed_cursor(&target.followed.session_id().to_string()))
        .unwrap();
    probe.passes.push((prefix, cursor));
    if probe.passes.len() == probe.rounds {
        probe.last_stream.release();
    }
}

struct Observation {
    acknowledgement: u64,
    admission_cursor: u64,
    delayed_tail: u64,
    passes: Vec<(u64, u64)>,
}

fn pause_admission(
    config: &mut crate::runtime::HostConfig<crate::runtime::test_kit::TestExtension>,
) -> (mpsc::Sender<()>, mpsc::Receiver<HostBoundary>) {
    let (boundaries, observed) = mpsc::channel();
    let (allow, allowed) = mpsc::channel();
    let allowed = Mutex::new(allowed);
    crate::runtime::test_kit::scripted_mut(config).observe = Arc::new(move |workflow, boundary| {
        if workflow == HostWorkflow::RootDelivery
            && matches!(
                boundary,
                HostBoundary::ProductPublished | HostBoundary::ProductCommitted
            )
        {
            boundaries.send(boundary).unwrap();
            allowed
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
        }
    });
    (allow, observed)
}

fn observe_delayed_turn(rounds: usize) -> Observation {
    let root = tempfile::tempdir().unwrap();
    let gate = Arc::new(GateHandle::new());
    let mut steps = Vec::new();
    if rounds == 2 {
        steps.push(serde_json::json!({
            "output": {"tool": {"name": "loopback_effect", "arguments": {}}},
            "finish": "tool_calls"
        }));
    }
    steps.push(serde_json::json!({"gate": gate.script(), "output": {"text": "done"}}));
    let (mut config, _) = scripted_host_with(root.path(), script(steps.into()));
    let id = config.session.id.clone();
    let store = config.store.clone();
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        &id,
        "root",
        "owner",
        config.cwd.clone(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let _runtime = crate::topology_runtime::insert_root(id.clone(), tree, None)
        .block()
        .unwrap();
    let (allow, observed) = pause_admission(&mut config);
    probes().lock().unwrap().insert(
        id.clone(),
        RoundProbe {
            passes: Vec::new(),
            rounds,
            last_stream: gate.clone(),
        },
    );
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    send_attached_action(
        &actions,
        UserAction::Prompt("observe delayed prefixes".into()),
    );
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(30)).unwrap(),
        HostBoundary::ProductPublished
    );
    // The host has the acknowledgement but cannot run its admission pass or
    // process ModelStarted. The independent owner reaches the last round.
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(30), gate.entered())
                .await
                .unwrap();
        })
        .unwrap();
    let locator = store.load(&id).unwrap().plakous_locator.unwrap();
    let feed = executor
        .block_on(crate::agent::read_tree_feed(
            &sessions,
            locator.session_id(),
            0,
        ))
        .unwrap()
        .unwrap();
    let acknowledgement = feed
        .records
        .iter()
        .find(|record| {
            record
                .facts()
                .iter()
                .any(|fact| matches!(fact, FeedFact::CommandAccepted { .. }))
        })
        .unwrap()
        .header()
        .seq
        .get();
    let delayed_tail = feed.records.last().unwrap().header().seq.get();
    allow.send(()).unwrap();
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(30)).unwrap(),
        HostBoundary::ProductCommitted
    );
    let admission_cursor = crate::topology_runtime::lookup(&id)
        .unwrap()
        .read(|tree| tree.feed_cursor(&locator.session_id().to_string()))
        .unwrap();
    allow.send(()).unwrap();
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the delayed turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let passes = probes().lock().unwrap().remove(&id).unwrap().passes;
    Observation {
        acknowledgement,
        admission_cursor,
        delayed_tail,
        passes,
    }
}

#[test]
fn delayed_model_started_processing_keeps_two_real_round_cursor_advances() {
    let observation = observe_delayed_turn(2);
    assert_eq!(observation.passes.len(), 2);
    let [(first_prefix, first_cursor), (second_prefix, second_cursor)] = observation.passes[..]
    else {
        unreachable!()
    };
    assert!(first_prefix < second_prefix);
    assert!(second_prefix <= observation.delayed_tail);
    assert_eq!(
        first_cursor, first_prefix,
        "the delayed first pass absorbed round two"
    );
    assert_eq!(second_cursor, second_prefix);
}

#[test]
fn admission_projection_stops_at_ack_even_after_the_first_round_commits() {
    let observation = observe_delayed_turn(1);
    assert!(observation.acknowledgement < observation.delayed_tail);
    assert_eq!(
        observation.admission_cursor, observation.acknowledgement,
        "the admission pass absorbed the already committed first round"
    );
}
