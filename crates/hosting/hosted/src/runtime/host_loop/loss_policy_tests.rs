//! The attached root forwards `[host] resume_after_loss` to its tree's hub.

#![allow(clippy::unwrap_used, reason = "test-owned host and tree")]
use crate::runtime::test_kit::BlockOn as _;

use crate::runtime::Sender;
use crate::runtime::recover_tests::{script, scripted_host_with};
use crate::runtime::test_kit::TestExtension;

/// A tree whose one child a lost host left held after a loss.
pub(in crate::runtime) fn held_child_tree(
    id: &str,
    cwd: std::path::PathBuf,
) -> (crate::agent_tree::ProductAgentTree, String) {
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        id,
        "root",
        "owner",
        cwd,
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent(
            "root",
            &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator".to_owned())
        .unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    // What a reopen's parked mark records.
    tree.mark_owner_lost(&child);
    assert_eq!(
        tree.loss_continuation(&child).map(|held| held.state),
        Some(crate::agent_tree::LossContinuationState::Held)
    );
    (tree, child)
}

/// The open forwards the host config's `resume_after_loss` to the tree's
/// hub, which is what every recovery run's continuation pass reads
/// (`a_continue_policy_resumes_a_held_child_once` pins the pass itself).
#[test]
fn an_attached_root_forwards_its_loss_policy_to_its_tree() {
    for continues in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
        config.resume_after_loss = if continues {
            crate::runtime::ResumeAfterLoss::Continue
        } else {
            crate::runtime::ResumeAfterLoss::Hold
        };
        let id = config.session.id.clone();
        let (tree, _child) = held_child_tree(&id, config.cwd.clone());
        let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None)
            .block()
            .unwrap();
        // The hub starts at the other policy, so only the open can set it.
        runtime.set_continue_after_loss(!continues);
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let host = super::RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        assert_eq!(runtime.continues_after_loss(), continues);
        drop(host);
        crate::topology_runtime::force_release(&id).block();
    }
}
