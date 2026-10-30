use super::CandidateId;
use crate::tree::AwaitAgents;
use crate::tree::test_support::{adopt_req, candidate_of, mint_req, spawn_req, tree, tree_at};
use crate::tree::{ListAgentsRequest, NodeStatus, WorkspaceStatus, WriterMode};

#[test]
fn child_retention_requires_delivery_and_sealed_work() {
    let (_tmp, mut tree) = tree();
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.prepare_child_retention(u64::MAX).unwrap(),
        [] as [crate::tree::retention::RetirementTicket; 0]
    );
    let result = tree
        .poll_wait("root", &AwaitAgents::Pending)
        .unwrap()
        .unwrap();
    tree.acknowledge_wait_mail("root", &result);
    assert_eq!(
        tree.prepare_child_retention(u64::MAX).unwrap()[0].agent_id,
        child.agent_id
    );
}
fn delivered(tree: &mut super::ProductAgentTree) {
    let result = tree
        .poll_wait("root", &AwaitAgents::Pending)
        .unwrap()
        .unwrap();
    tree.acknowledge_wait_mail("root", &result);
}

#[test]
fn child_retention_bounds_records_and_never_reuses_identity_or_refunds_budget() {
    let (_tmp, mut tree) = tree();
    tree.snapshot.child_retention_count = 1;
    let first = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_completed(&first.agent_id).unwrap();
    tree.settle_for_test();
    delivered(&mut tree);
    let second = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_completed(&second.agent_id).unwrap();
    tree.settle_for_test();
    delivered(&mut tree);
    let spent = tree.attempts_spent();
    let tickets = tree
        .prepare_child_retention(super::super::now_unix_millis())
        .unwrap();
    assert_eq!(tickets.len(), 1);
    assert_eq!(tickets[0].agent_id, first.agent_id);
    tree.finish_child_retention(&tickets[0]).unwrap();
    assert!(tree.node(&first.agent_id).is_err());
    assert!(tree.node(&second.agent_id).is_ok());
    assert_eq!(tree.attempts_spent(), spent);
    let next = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_ne!(next.agent_id, first.agent_id);
    assert_ne!(next.agent_id, second.agent_id);
}

#[test]
fn child_retention_keeps_unadopted_work_listable_adoptable_and_as_spawn_input() {
    let repo = super::super::dormancy::tests::git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(path.join("WORK.txt"), "retained work\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    delivered(&mut tree);
    let candidate = candidate_of(&tree, &child.agent_id);
    tree.reclaim_dormant(u64::MAX, &std::collections::BTreeMap::new())
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.workspaces[&tree.nodes[&child.agent_id].workspace_id].status,
        WorkspaceStatus::CleanRemoved
    );
    let ticket = tree.prepare_child_retention(u64::MAX).unwrap().remove(0);
    tree.finish_child_retention(&ticket).unwrap();
    tree.clear_feed_views();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert_eq!(tree.node(&child.agent_id).unwrap().prompt, "");
    assert!(
        tree.resolve_input_base(&[candidate.to_string()])
            .unwrap()
            .is_some()
    );
    let listing = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: None,
                include_completed: Some(true),
            },
        )
        .unwrap();
    assert!(
        serde_json::to_string(&listing)
            .unwrap()
            .contains("conversation_retired")
    );
    assert!(
        serde_json::to_string(&listing)
            .unwrap()
            .contains(candidate.as_str())
    );
    tree.adopt_candidate("root", &adopt_req(&candidate), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("WORK.txt")).unwrap(),
        "retained work\n"
    );
}

#[test]
fn collected_records_keep_durable_pin_cleanup_until_acknowledged() {
    use crate::tree::{MemoryTreeStore, ProductAgentTree, TreeStore as _};
    let (_tmp, _vcs, mut tree) = crate::tree::test_support::fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let rescue = CandidateId::from_persisted("failed-c1-a1-turn".into());
    tree.record_rescue_pin(&child.agent_id, rescue.clone())
        .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    delivered(&mut tree);
    let candidate = candidate_of(&tree, &child.agent_id);
    tree.adopt_candidate("root", &adopt_req(&candidate), None)
        .unwrap();
    tree.close_agent(
        "root",
        &crate::tree::test_support::close_req(&child.agent_id),
        None,
    )
    .unwrap();
    tree.settle_for_test();
    let ticket = tree.prepare_child_retention(u64::MAX).unwrap().remove(0);
    tree.finish_child_retention(&ticket).unwrap();
    assert!(!tree.candidates.contains_key(&candidate));
    let store = MemoryTreeStore::new();
    let revision = store.save(&tree).unwrap();
    tree.acknowledge_revision(revision);
    let mut reopened = ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
    let mut pending = reopened.pending_pin_cleanup().unwrap();
    assert_eq!(pending.len(), 1);
    let mut cleanup = pending.remove(0);
    let owned = vec![candidate, rescue];
    assert_eq!(cleanup.candidates, owned);
    cleanup.candidates.clear();
    assert!(reopened.acknowledge_pin_cleanup(&cleanup).is_err());
    assert_eq!(reopened.pending_pin_cleanup().unwrap().len(), 1);
    cleanup.candidates = owned;
    reopened.acknowledge_pin_cleanup(&cleanup).unwrap();
    assert!(reopened.pending_pin_cleanup().unwrap().is_empty());
}

#[test]
fn child_retention_intent_is_idempotent_and_close_is_immediate() {
    let (_tmp, mut tree) = tree();
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    delivered(&mut tree);
    tree.nodes.get_mut(&child.agent_id).unwrap().status = NodeStatus::Closed;
    let ticket = tree.prepare_child_retention(0).unwrap().remove(0);
    assert_eq!(
        tree.prepare_child_retention(0).unwrap(),
        vec![ticket.clone()]
    );
    tree.finish_child_retention(&ticket).unwrap();
    tree.finish_child_retention(&ticket).unwrap();
    assert_eq!(
        tree.prepare_child_retention(u64::MAX).unwrap(),
        [] as [crate::tree::retention::RetirementTicket; 0]
    );
}
