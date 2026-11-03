use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use plakous_host_protocol::selection::CompositionSelection;

use super::{ForkedFrom, NodeLineId, NodePlacement, NodeWorkspace, TreeEpochs};
use crate::tree::test_support::{spawn_req, tree};
use crate::tree::{
    MemoryTreeStore, ProductAgentTree, TopologyErrorCode, TreeAdmissionState, TreeStore,
};

const LINE: &str = "0190c3a1-5b7e-7c2d-9f00-0123456789ab";

#[test]
fn a_line_id_is_canonical_non_nil_uuid_text() {
    assert_eq!(NodeLineId::parse(LINE).unwrap().as_str(), LINE);
    for refused in [
        "",
        "0190C3A1-5B7E-7C2D-9F00-0123456789AB",
        "00000000-0000-0000-0000-000000000000",
        "0190c3a15b7e7c2d9f000123456789ab",
        "0190c3a1-5b7e-7c2d-9f00-0123456789ag",
        "0190c3a1_5b7e-7c2d-9f00-0123456789ab",
    ] {
        assert!(NodeLineId::parse(refused).is_err(), "{refused}");
    }
    let encoded = serde_json::to_string(&NodeLineId::parse(LINE).unwrap()).unwrap();
    assert_eq!(encoded, format!("\"{LINE}\""));
    assert!(serde_json::from_str::<NodeLineId>("\"not-a-line\"").is_err());
}

/// Today's flows write only the defaults: epoch 0, no line, the local
/// runner, no recorded parent composition, no provenance, no ancestors.
#[test]
fn todays_trees_carry_only_the_defaults() {
    let (_root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    for id in ["root", child.as_str()] {
        let node = tree.node(id).unwrap();
        assert_eq!(node.line_id, None);
        assert_eq!(node.epoch_no, 0);
        assert!(node.placement.is_local());
        assert_eq!(node.parent_composition, None);
    }
    assert_eq!(tree.epochs(), &TreeEpochs::default());
    assert_eq!(tree.forked_from(), None);
    assert!(tree.runner_ancestors().is_empty());
}

#[test]
fn epoch_rows_are_coherent_only_with_held_references() {
    let (_root, tree) = tree();
    let mut epochs = TreeEpochs::default();
    assert!(epochs.is_coherent() && epochs.holds(0) && !epochs.holds(1));
    epochs.current = 1;
    assert!(!epochs.is_coherent(), "current names no epoch");
    epochs.successors.insert(1, tree.snapshot.clone());
    assert!(epochs.is_coherent());
    let mut edited = tree.snapshot.clone();
    edited.transient_resumes = 2;
    epochs.successors.insert(2, edited);
    assert!(!epochs.is_coherent(), "a digest that does not match");
    epochs.successors.remove(&2);
    epochs.successors.insert(0, tree.snapshot.clone());
    assert!(!epochs.is_coherent(), "successors are numbered from 1");
}

/// Every lineage row and node column round-trips through the in-memory
/// store and the reopen law leaves them alone.
#[test]
fn lineage_rows_round_trip_through_the_memory_store() {
    let (_root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let mut successor = tree.snapshot.clone();
    successor.transient_resumes = 2;
    successor.digest = successor.compute_digest();
    tree.lineage.epochs = TreeEpochs {
        current: 1,
        successors: BTreeMap::from([(1, successor)]),
    };
    tree.lineage.forked_from = Some(ForkedFrom::Node {
        tree_id: "source".to_owned(),
        agent_id: "root".to_owned(),
        line_id: NodeLineId::parse(LINE).unwrap(),
        branch_id: LINE.to_owned(),
        epoch_no: 0,
    });
    tree.lineage.runner_ancestors = BTreeMap::from([(
        "runner-1".to_owned(),
        BTreeSet::from(["abc".to_owned(), "def".to_owned()]),
    )]);
    let node = tree.nodes.get_mut(&child).unwrap();
    node.line_id = Some(NodeLineId::parse(LINE).unwrap());
    node.epoch_no = 1;
    node.placement = NodePlacement {
        runner_id: Some("runner-1".to_owned()),
        workspace: Some(NodeWorkspace {
            root: PathBuf::from("/srv/copies/t/c1"),
            cwd: PathBuf::from("/srv/copies/t/c1/src"),
        }),
    };
    node.parent_composition = Some(CompositionSelection::NativePtc);

    let store = MemoryTreeStore::new();
    let _ = store.save(&tree).unwrap();
    let reopened = ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
    assert_eq!(reopened.epochs(), tree.epochs());
    assert_eq!(reopened.forked_from(), tree.forked_from());
    assert_eq!(reopened.runner_ancestors(), tree.runner_ancestors());
    assert_eq!(reopened.node(&child).unwrap(), tree.node(&child).unwrap());

    let imported = ForkedFrom::Import {
        provenance: "plak export v12 of session s".to_owned(),
    };
    let encoded = serde_json::to_string(&imported).unwrap();
    assert_eq!(
        encoded,
        r#"{"import":{"provenance":"plak export v12 of session s"}}"#
    );
    assert_eq!(
        serde_json::from_str::<ForkedFrom>(&encoded).unwrap(),
        imported
    );
}

/// The current epoch with `edit` applied and its digest recomputed.
fn successor_of(
    tree: &ProductAgentTree,
    edit: impl FnOnce(&mut crate::epoch::OrchestrationSnapshot),
) -> crate::epoch::OrchestrationSnapshot {
    let mut successor = tree.current_epoch().clone();
    edit(&mut successor);
    successor.digest = successor.compute_digest();
    successor
}

fn template(name: &str) -> crate::protocol::TemplateName {
    crate::protocol::TemplateName::parse(name).unwrap()
}

/// Every reader resolves to the one header snapshot while a tree is on
/// epoch 0, which is every tree today.
#[test]
fn every_accessor_reads_the_header_on_epoch_zero() {
    let (_root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let header = tree.header_snapshot();
    assert!(std::ptr::eq(tree.current_epoch(), header));
    for id in ["root", child.as_str()] {
        assert!(std::ptr::eq(tree.epoch_of(id).unwrap(), header));
        assert!(std::ptr::eq(tree.caller_epoch(id).unwrap(), header));
        assert!(Arc::ptr_eq(
            &tree.cell_budget(id).unwrap(),
            &tree.cell_budget("root").unwrap()
        ));
    }
    assert_eq!(tree.retained_epochs().count(), 1);
    assert!(tree.epoch_digests_match());
    assert!(tree.epoch_of("nobody").is_none());
    assert_eq!(
        tree.caller_epoch("nobody").unwrap_err().code,
        TopologyErrorCode::GrantDenied
    );
}

/// An append moves the root and future root spawns to the new epoch while
/// a live child keeps its own: its cards, its turn ceiling and its cell
/// budget. The admission state is kept.
#[test]
fn an_append_moves_the_root_and_keeps_live_children_on_their_epoch() {
    let (_root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let child_budget = tree.cell_budget(&child).unwrap();
    tree.state = TreeAdmissionState::TreeTerminal;
    let successor = successor_of(&tree, |epoch| {
        epoch.root_tuple = "successor-tuple".to_owned();
        epoch.working_set.remove(&template("review"));
        epoch.roster.remove(&template("review"));
        epoch
            .roster
            .get_mut(&template("explore"))
            .unwrap()
            .max_turns = Some(3);
        epoch.max_agents_per_cell = 2;
    });

    assert_eq!(tree.append_epoch(successor.clone()).unwrap(), 1);

    assert_eq!(tree.state, TreeAdmissionState::TreeTerminal);
    assert_eq!(tree.epochs().current, 1);
    assert_eq!(tree.current_epoch(), &successor);
    assert_eq!(tree.selected_tuple, "successor-tuple");
    assert_eq!(tree.node("root").unwrap().epoch_no, 1);
    assert_eq!(tree.node("root").unwrap().tuple_id, "successor-tuple");
    assert_eq!(tree.caller_epoch("root").unwrap(), &successor);
    // The live child keeps epoch 0: its card is still there for its reopen.
    assert_eq!(tree.node(&child).unwrap().epoch_no, 0);
    let header = tree.header_snapshot();
    assert!(std::ptr::eq(tree.epoch_of(&child).unwrap(), header));
    assert!(
        tree.epoch_of(&child)
            .unwrap()
            .binding(&template("review"))
            .is_some()
    );
    assert!(Arc::ptr_eq(
        &tree.cell_budget(&child).unwrap(),
        &child_budget
    ));
    assert_eq!(tree.cell_budget("root").unwrap().budget(), 2);
    assert!(tree.epoch_digests_match());

    // Root spawns bind the new epoch: `review` is gone from it, and an
    // `explore` child is stamped with epoch 1 and its turn ceiling.
    tree.state = TreeAdmissionState::Live;
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::TemplateUnknown
    );
    let explore = crate::protocol::SpawnAgentRequest::test_named(
        "explore",
        crate::protocol::Tier::ReadOnly,
        "scout",
        "look",
    );
    let second = tree.spawn_agent("root", &explore, None).unwrap().agent_id;
    assert_eq!(tree.node(&second).unwrap().epoch_no, 1);
    assert_eq!(tree.attempt_turn_ceiling(&second), Some(3));
    assert_eq!(tree.attempt_turn_ceiling(&child), None);

    // Both epochs survive a store round trip, and so does each node's.
    let store = MemoryTreeStore::new();
    let _ = store.save(&tree).unwrap();
    let reopened = ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
    assert_eq!(reopened.epochs(), tree.epochs());
    assert_eq!(reopened.epoch_of(&child), tree.epoch_of(&child));
    assert_eq!(reopened.epoch_of(&second), Some(&successor));
    assert_eq!(reopened.cell_budget("root").unwrap().budget(), 2);
}

#[test]
fn an_append_refuses_pending_spawns_and_a_stale_digest() {
    let (_root, mut tree) = tree();
    let successor = successor_of(&tree, |epoch| epoch.transient_resumes = 2);
    let mut stale = successor.clone();
    stale.transient_resumes = 0;
    assert_eq!(
        tree.append_epoch(stale).unwrap_err().code,
        TopologyErrorCode::SchemaMismatch
    );
    tree.set_pending_spawns_for_test(1);
    let refused = tree.append_epoch(successor.clone()).unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::GrantDenied);
    assert_eq!(
        refused.message,
        ProductAgentTree::TREE_EPOCH_REBIND_PENDING_SPAWNS
    );
    assert_eq!(tree.epochs(), &TreeEpochs::default());
    tree.set_pending_spawns_for_test(0);
    assert_eq!(tree.append_epoch(successor).unwrap(), 1);
}

/// A successor epoch no node references is folded at the next append;
/// epoch 0 is the header and stays.
#[test]
fn an_append_folds_epochs_no_node_references() {
    let (_root, mut tree) = tree();
    let first = successor_of(&tree, |epoch| epoch.transient_resumes = 2);
    assert_eq!(tree.append_epoch(first).unwrap(), 1);
    let second = successor_of(&tree, |epoch| epoch.transient_resumes = 0);
    assert_eq!(tree.append_epoch(second.clone()).unwrap(), 2);
    assert_eq!(
        tree.epochs().successors.keys().copied().collect::<Vec<_>>(),
        [2]
    );
    assert_eq!(
        tree.retained_epochs()
            .map(|(number, _)| number)
            .collect::<Vec<_>>(),
        [0, 2]
    );
    assert_eq!(tree.current_epoch(), &second);
}

/// At most eight epochs are referenced at once: the current one and every
/// epoch a child was spawned under.
#[test]
fn an_append_past_eight_referenced_epochs_is_refused() {
    let (_root, mut tree) = tree();
    for epoch in 0..super::MAX_REFERENCED_EPOCHS as u64 {
        let child = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        assert_eq!(tree.node(&child).unwrap().epoch_no, epoch);
        let successor = successor_of(&tree, |snapshot| {
            snapshot.root_tuple = format!("tuple-{epoch}");
        });
        let appended = tree.append_epoch(successor);
        if epoch + 1 < super::MAX_REFERENCED_EPOCHS as u64 {
            assert_eq!(appended.unwrap(), epoch + 1);
        } else {
            assert_eq!(appended.unwrap_err().code, TopologyErrorCode::BoundExceeded);
        }
    }
    assert_eq!(tree.epochs().current, 7);
    assert_eq!(tree.retained_epochs().count(), 8);
}

/// A fork's genesis is a tree of its own: its root is a new node named after
/// the tree, its epoch 0 is the source's current epoch byte for byte, it
/// records where it came from, and nothing of the source's work (children,
/// candidates) comes with it, so a source candidate is not adoptable there.
#[test]
fn a_fork_genesis_is_an_independent_tree_seeded_with_the_source_epoch() {
    use crate::tree::test_support::{adopt_req, candidate_of, fake_tree, mint_req};
    let (root, _vcs, mut source) = fake_tree();
    let child = source.spawn_agent("root", &mint_req(), None).unwrap();
    source.record_turn_completed(&child.agent_id).unwrap();
    source.settle_for_test();
    let candidate = candidate_of(&source, &child.agent_id);
    let line = NodeLineId::parse(LINE).unwrap();
    let mut fork = crate::tree::block_on_for_test(ProductAgentTree::fork_genesis(
        &source,
        "fork1",
        root.path().to_path_buf(),
        line.clone(),
        "0190c3a1-5b7e-7c2d-9f00-00000000b001".to_owned(),
    ))
    .unwrap();

    assert_eq!(
        (fork.tree_id.as_str(), fork.root_agent_id.as_str()),
        ("fork1", "fork1")
    );
    assert_eq!(fork.node("fork1").unwrap().epoch_no, 0);
    assert!(fork.node(&child.agent_id).is_err());
    assert_eq!(
        serde_json::to_string(fork.current_epoch()).unwrap(),
        serde_json::to_string(source.current_epoch()).unwrap()
    );
    assert_eq!(
        fork.forked_from(),
        Some(&ForkedFrom::Node {
            tree_id: "tree1".to_owned(),
            agent_id: "root".to_owned(),
            line_id: line,
            branch_id: "0190c3a1-5b7e-7c2d-9f00-00000000b001".to_owned(),
            epoch_no: 0,
        })
    );
    assert!(fork.candidates().is_empty());
    let refused = fork
        .adopt_candidate("fork1", &adopt_req(&candidate), None)
        .unwrap_err();
    assert_ne!(refused.code, TopologyErrorCode::MergeConflict);
    assert!(source.candidates().contains_key(&candidate));

    // Persisted, both trees reopen independently with their own rows.
    let store = MemoryTreeStore::new();
    let _source_revision = store.save(&source).unwrap();
    let _fork_revision = store.save(&fork).unwrap();
    let reopened = ProductAgentTree::reopen(store.load("fork1").unwrap().unwrap());
    assert_eq!(reopened.forked_from(), fork.forked_from());
    assert_eq!(
        serde_json::to_string(reopened.current_epoch()).unwrap(),
        serde_json::to_string(source.current_epoch()).unwrap()
    );
    assert_eq!(reopened.nodes().len(), 1);
    let source_again = ProductAgentTree::reopen(store.load("tree1").unwrap().unwrap());
    assert!(source_again.forked_from().is_none());
    assert!(source_again.node(&child.agent_id).is_ok());
}
