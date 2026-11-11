use super::test_support::*;
use super::*;
use plakous_workspace::occupancy_vcs::fake::FakeOp;

#[tokio::test]
async fn placement_hold_routed_local_home_publishes_remote_seal_before_dispatch_without_a_live_route()
 {
    use crate::tree::TreeStore as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_root, vcs, mut tree) = fake_tree();
    let local = tree
        .spawn_agent("root", &mint_req(), None)
        .unwrap()
        .agent_id;
    let remote = tree
        .spawn_agent("root", &mint_req(), None)
        .unwrap()
        .agent_id;
    tree.nodes.get_mut(&remote).unwrap().placement.runner_id = Some("remote-child".into());
    for id in [&local, &remote] {
        let workspace = tree.node(id).unwrap().workspace_id.clone();
        tree.workspaces.get_mut(&workspace).unwrap().base_commit = Some("a".repeat(40));
    }
    tree.record_turn_completed(&local).unwrap();
    tree.record_turn_completed(&remote).unwrap();
    let effects = tree.take_effects();
    let tree_id = tree.tree_id.clone();
    // The route was dropped: the live port now reports neither placement nor runner.
    let routed = Arc::new(crate::routed_vcs::RoutedVcs::new(vcs.clone(), &tree_id, vec![]).await);
    let store = Arc::new(MemoryTreeStore::new());
    let staged = Arc::new(AtomicUsize::new(0));
    vcs.watch_lock({
        let (store, staged) = (store.clone(), staged.clone());
        move || {
            if let Some(stage) = plakous_workspace::occupancy_vcs::stage::current() {
                let stored = store.load(&tree_id).unwrap().unwrap().0;
                let operation = stored
                    .operations()
                    .get(stage.operation_id())
                    .expect("stage published before dispatch");
                let intent: serde_json::Value =
                    serde_json::from_str(&operation.canonical_request).unwrap();
                assert_eq!(intent["target_runner"], "remote-child");
                staged.fetch_add(1, Ordering::SeqCst);
            }
            false
        }
    });
    let tree = Arc::new(std::sync::Mutex::new(tree));
    let driver = crate::tools::TreeDriver::new(tree.clone(), store, routed, None);
    driver.save(&mut tree.lock().unwrap()).unwrap();
    driver.settle(effects).await.unwrap();
    assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 2);
    assert_eq!(
        staged.load(Ordering::SeqCst),
        1,
        "only the persisted remote seal is staged"
    );
}

#[test]
fn placement_hold_defers_seal_without_mail_or_parent_wait_and_isolates_siblings() {
    let (_root, vcs, mut tree) = fake_tree();
    let blocked = tree
        .spawn_agent("root", &mint_req(), None)
        .unwrap()
        .agent_id;
    let sibling = tree
        .spawn_agent("root", &mint_req(), None)
        .unwrap()
        .agent_id;
    let path = tree.node(&blocked).unwrap().path.clone();
    vcs.set_placement_ready(path.clone(), false);
    tree.record_turn_completed(&blocked).unwrap();
    assert!(
        tree.take_effects().is_empty(),
        "absent placement dispatches no Tree seal"
    );
    assert_eq!(tree.node(&blocked).unwrap().status, NodeStatus::Sealing);
    assert_eq!(tree.pending_mail("root").len(), 0);
    assert!(
        tree.poll_wait("root", &wait_for(&blocked))
            .unwrap()
            .is_none()
    );
    assert!(tree.live_child_work("root").contains(&blocked));
    tree.record_turn_completed(&sibling).unwrap();
    tree.settle_for_test();
    assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
    assert_eq!(tree.pending_mail("root").len(), 1);
    assert!(
        tree.poll_wait("root", &wait_for(&blocked))
            .unwrap()
            .is_none()
    );
    vcs.set_placement_ready(path, true);
    tree.settle_for_test();
    assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 2);
    assert_eq!(tree.pending_mail("root").len(), 2);
    assert!(
        tree.poll_wait("root", &wait_for(&blocked))
            .unwrap()
            .is_some()
    );
    tree.settle_for_test();
    assert_eq!(
        vcs.calls_of(FakeOp::Seal).len(),
        2,
        "release seals exactly once"
    );
}

#[test]
fn placement_hold_unstarted_seal_reopens_as_debt_but_staged_seal_never_restarts() {
    for staged in [false, true] {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree
            .spawn_agent("root", &mint_req(), None)
            .unwrap()
            .agent_id;
        let path = tree.node(&child).unwrap().path.clone();
        tree.nodes.get_mut(&child).unwrap().placement.runner_id = Some("placed".into());
        let workspace = tree.node(&child).unwrap().workspace_id.clone();
        tree.workspaces.get_mut(&workspace).unwrap().base_commit = Some("a".repeat(40));
        vcs.set_placement_ready(path.clone(), true);
        tree.record_turn_completed(&child).unwrap();
        if staged {
            let effects = tree.take_effects();
            tree.record_effect_stages(&effects, "placed").unwrap();
        }
        vcs.set_placement_ready(path.clone(), false);
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        reopened.set_vcs(vcs.clone());
        assert!(reopened.take_effects().seals.is_empty());
        assert!(reopened.candidates().is_empty());
        assert_eq!(reopened.pending_mail("root").len(), 0);
        if staged {
            assert_eq!(
                reopened.node(&child).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            vcs.set_placement_ready(path, true);
            assert!(
                reopened.take_effects().seals.is_empty(),
                "uncertain seal is not an unstarted debt"
            );
        } else {
            assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Sealing);
            let mut reopened = ProductAgentTree::reopen(reopened.to_persisted());
            reopened.set_vcs(vcs.clone());
            vcs.set_placement_ready(path, true);
            assert_eq!(reopened.take_effects().seals.len(), 1);
            assert!(reopened.take_effects().seals.is_empty());
        }
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
    }
}

#[test]
fn placement_hold_parked_wake_keeps_mail_unread_and_spends_no_turn_or_nudge() {
    let (_root, vcs, mut tree) = fake_tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.park_child(&child, vec![]).unwrap();
    let path = tree.node(&child).unwrap().path.clone();
    vcs.set_placement_ready(path.clone(), false);
    tree.send_agent_message("root", &mail(&child, "continue"), None)
        .unwrap();
    let before = tree.attempt(&child).unwrap().clone();
    assert!(
        !tree.begin_wake(&child),
        "placement gates the wake reservation"
    );
    assert!(tree.is_parked(&child));
    assert_eq!(tree.wake_mail(&child).len(), 1);
    assert_eq!(tree.attempt(&child).unwrap(), &before);
    vcs.set_placement_ready(path.clone(), true);
    assert!(tree.begin_wake(&child));
    vcs.set_placement_ready(path, false);
    assert!(
        !tree.wake_admissible(&child),
        "bind rechecks a loss after reservation"
    );
    tree.defer_wake(&child);
    assert!(tree.is_parked(&child));
    assert_eq!(tree.wake_mail(&child).len(), 1);
}
