//! The root's tree passes run at exactly the four mandated points.

#![allow(clippy::unwrap_used, reason = "test-owned host and tree")]
use crate::runtime::test_kit::BlockOn as _;

use std::time::{Duration, Instant};

use crate::runtime::Sender;
use crate::runtime::projection::RootTreePass;
use crate::runtime::projection::probe::{refuse_root_tree_passes, root_tree_passes_of};
use crate::runtime::recover_tests::{script, scripted_host_with, turn_completed, wait_for};
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::send_attached_action;
use crate::surface_protocol::UserAction;

/// Waits until the recorded passes satisfy `done`.
fn passes_until(id: &str, what: &str, done: impl Fn(&[RootTreePass]) -> bool) -> Vec<RootTreePass> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let passes = root_tree_passes_of(id);
        if done(&passes) {
            return passes;
        }
        assert!(Instant::now() < deadline, "{what}: {passes:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn insert_tree(
    config: &crate::runtime::HostConfig<TestExtension>,
) -> std::sync::Arc<crate::topology_runtime::ProductTopologyRuntime> {
    let id = config.session.id.clone();
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        &id,
        "root",
        "owner",
        config.cwd.clone(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    crate::topology_runtime::insert_root(id, tree, None)
        .block()
        .unwrap()
}

/// One two-round turn runs the admission pass at the tail, the
/// post-acknowledgement pass, one pass per round at its `ModelStarted`
/// prefix and the settled-turn pass. Idle ticks and intents alone run none;
/// release runs the idle-entry pass.
#[test]
fn root_tree_passes_run_at_the_four_points_and_nowhere_else() {
    let root = tempfile::tempdir().unwrap();
    let steps = serde_json::json!([
        {
            "output": {"tool": {"name": "loopback_effect", "arguments": {}}},
            "finish": "tool_calls"
        },
        {"output": {"text": "done"}}
    ]);
    let (config, _) = scripted_host_with(root.path(), script(steps));
    let id = config.session.id.clone();
    let _runtime = insert_tree(&config);
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    assert!(
        root_tree_passes_of(&id).is_empty(),
        "an unstored session opens with no pass"
    );

    send_attached_action(&actions, UserAction::Prompt("two rounds".into()));
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the two-round turn", turn_completed);
    let turn = passes_until(&id, "the settled-turn pass", |passes| {
        passes.last() == Some(&RootTreePass::Settled)
    });
    assert!(
        matches!(
            turn[..],
            [
                RootTreePass::Admission,
                RootTreePass::Acknowledged { through: ack },
                RootTreePass::Round { through: first },
                RootTreePass::Round { through: second },
                RootTreePass::Settled,
            ] if ack <= first && first < second
        ),
        "{turn:?}"
    );

    // Several idle observation windows, and intents that are not prompts:
    // none of them is a pass point.
    std::thread::sleep(Duration::from_millis(500));
    for _ in 0..3 {
        send_attached_action(&actions, UserAction::RefreshTemplates);
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        root_tree_passes_of(&id),
        turn,
        "an idle tick or intent ran a pass"
    );

    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let released = root_tree_passes_of(&id);
    assert_eq!(released.len(), turn.len() + 1, "{released:?}");
    assert_eq!(released.last(), Some(&RootTreePass::Settled));
}

/// A refused idle-entry pass stays due, so the next idle tick runs it again,
/// once; a tick after it succeeded runs none.
#[test]
fn a_refused_idle_entry_pass_is_retried_by_the_next_tick() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
    let id = config.session.id.clone();
    let _runtime = insert_tree(&config);
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = super::RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    let opened = root_tree_passes_of(&id).len();

    refuse_root_tree_passes(&id, 1);
    host.role.idle_entry_pass_due = true;
    host.block(async |host| host.project().await);
    assert!(
        host.role.idle_entry_pass_due,
        "the refused pass is due again"
    );
    host.block(async |host| host.project().await);
    assert!(!host.role.idle_entry_pass_due, "the retried pass succeeded");
    host.block(async |host| host.project().await);

    assert_eq!(
        root_tree_passes_of(&id)[opened..],
        [RootTreePass::Settled, RootTreePass::Settled]
    );
}
