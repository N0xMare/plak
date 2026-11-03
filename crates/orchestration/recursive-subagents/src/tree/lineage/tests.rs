use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use plakous_host_protocol::selection::CompositionSelection;

use super::{ForkedFrom, NodeLineId, NodePlacement, NodeWorkspace, TreeEpochs};
use crate::tree::test_support::{
    adopt_req, candidate_of, close_req, fake_tree, mint_req, spawn_req, tree, tree_at,
};
use crate::tree::vcs_fixtures::git_repo;
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
    let _ = store.save_in_custody(&tree).unwrap();
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
    let header = tree.epoch_snapshot(0).unwrap();
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
    let header = tree.epoch_snapshot(0).unwrap();
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
    let _ = store.save_in_custody(&tree).unwrap();
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
/// epoch a child was spawned under. A closed child still counts, because a
/// followup can revive it on its own epoch, so the refusal points at a new
/// session rather than at closing agents.
#[test]
fn an_append_past_eight_referenced_epochs_is_refused() {
    let (_root, mut tree) = tree();
    for epoch in 0..super::MAX_REFERENCED_EPOCHS as u64 {
        let child = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        assert_eq!(tree.node(&child).unwrap().epoch_no, epoch);
        tree.close_agent("root", &close_req(&child), None).unwrap();
        tree.settle_for_test();
        assert!(tree.node(&child).unwrap().status.is_finished());
        let successor = successor_of(&tree, |snapshot| {
            snapshot.root_tuple = format!("tuple-{epoch}");
        });
        let appended = tree.append_epoch(successor);
        if epoch + 1 < super::MAX_REFERENCED_EPOCHS as u64 {
            assert_eq!(appended.unwrap(), epoch + 1);
        } else {
            let refused = appended.unwrap_err();
            assert_eq!(refused.code, TopologyErrorCode::BoundExceeded);
            assert!(refused.message.contains("until retention removes it"));
            assert!(!refused.message.contains("close"), "{}", refused.message);
        }
    }
    assert_eq!(tree.epochs().current, 7);
    assert_eq!(tree.retained_epochs().count(), 8);
}

/// A switch's pending epoch is refused when its append would be: with eight
/// epochs referenced, recording a ninth writes no row.
#[test]
fn a_pending_epoch_past_the_bound_is_not_recorded() {
    let (_root, mut tree) = tree();
    for epoch in 0..super::MAX_REFERENCED_EPOCHS as u64 {
        let child = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.close_agent("root", &close_req(&child), None).unwrap();
        tree.settle_for_test();
        if epoch + 1 < super::MAX_REFERENCED_EPOCHS as u64 {
            let successor = successor_of(&tree, |snapshot| {
                snapshot.root_tuple = format!("tuple-{epoch}");
            });
            tree.append_epoch(successor).unwrap();
        }
    }
    let ninth = successor_of(&tree, |snapshot| snapshot.root_tuple = "ninth".into());
    let refused = tree.record_epoch_pending("switch-9", &ninth).unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::BoundExceeded);
    assert_eq!(tree.epoch_pending().unwrap(), Vec::new());
    assert_eq!(tree.epochs().current, 7);
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
    let _source_revision = store.save_in_custody(&source).unwrap();
    let _fork_revision = store.save_in_custody(&fork).unwrap();
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

/// `orchestrate`, read-only by default, as a forked mutating lead.
fn lead_req() -> crate::protocol::SpawnAgentRequest {
    let mut request = crate::protocol::SpawnAgentRequest::test_orchestrate();
    request.tier = crate::protocol::Tier::Mutate;
    request.workspace = crate::protocol::SpawnWorkspace::Fork;
    request
}

/// A non-root caller still on an older epoch spawns from its own epoch's
/// roster after an append changed every value it depends on: the card it
/// binds, the epoch its child is stamped with, its own tool names, its
/// child's delegation authority, and its cell budget.
#[test]
fn a_caller_on_an_older_epoch_spawns_from_its_own_roster() {
    let (_root, mut tree) = tree();
    let mut genesis = tree.current_epoch().clone();
    genesis.max_depth = 3;
    genesis.max_agents_per_cell = 2;
    genesis.roster.get_mut(&template("explore")).unwrap().spawns = vec![template("review")];
    genesis.digest = genesis.compute_digest();
    tree.install_snapshot(genesis);
    let lead = tree
        .spawn_agent(
            "root",
            &crate::protocol::SpawnAgentRequest::test_orchestrate(),
            None,
        )
        .unwrap()
        .agent_id;
    let successor = successor_of(&tree, |epoch| {
        epoch.working_set.remove(&template("review"));
        epoch.roster.remove(&template("review"));
        for name in ["orchestrate", "explore"] {
            epoch
                .roster
                .get_mut(&template(name))
                .unwrap()
                .spawns
                .clear();
        }
        epoch.max_agents_per_cell = 5;
    });
    assert_eq!(tree.append_epoch(successor).unwrap(), 1);

    // Its tool names come from its own epoch's menu.
    assert!(
        tree.compiled_tool_names(&lead)
            .unwrap()
            .contains(&"spawn_agent")
    );
    // It binds a card the current epoch no longer carries, and its child is
    // stamped with the lead's epoch.
    let reviewer = tree
        .spawn_agent(&lead, &spawn_req(), None)
        .unwrap()
        .agent_id;
    assert_eq!(tree.node(&reviewer).unwrap().epoch_no, 0);
    // The child's delegation authority is its template's menu in that epoch.
    let explore = crate::protocol::SpawnAgentRequest::test_named(
        "explore",
        crate::protocol::Tier::ReadOnly,
        "scout",
        "look",
    );
    let scout = tree.spawn_agent(&lead, &explore, None).unwrap().agent_id;
    assert_eq!(tree.node(&scout).unwrap().epoch_no, 0);
    assert!(tree.grants()[&scout].may_spawn);
    // Its cells spend its epoch's budget; the root's follow the current one.
    assert_eq!(tree.cell_budget(&lead).unwrap().budget(), 2);
    assert_eq!(tree.cell_budget("root").unwrap().budget(), 5);
    // The root itself spawns from the current epoch.
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::TemplateUnknown
    );
}

/// A child spawned under a successor epoch keeps that epoch across a further
/// append: its turn ceiling, its mint hook and its cell budget.
#[test]
fn a_child_on_a_successor_epoch_keeps_it_across_a_further_append() {
    let (_root, _vcs, mut tree) = fake_tree();
    let first = successor_of(&tree, |epoch| {
        let card = epoch.roster.get_mut(&template("implement")).unwrap();
        card.max_turns = Some(3);
        card.on_mint = Some("echo one".to_owned());
        epoch.max_agents_per_cell = 2;
    });
    assert_eq!(tree.append_epoch(first).unwrap(), 1);
    let child = tree
        .spawn_agent("root", &mint_req(), None)
        .unwrap()
        .agent_id;
    assert_eq!(tree.node(&child).unwrap().epoch_no, 1);
    let budget = tree.cell_budget(&child).unwrap();
    let second = successor_of(&tree, |epoch| {
        let card = epoch.roster.get_mut(&template("implement")).unwrap();
        card.max_turns = Some(5);
        card.on_mint = Some("echo two".to_owned());
        epoch.max_agents_per_cell = 4;
    });
    assert_eq!(tree.append_epoch(second).unwrap(), 2);

    assert_eq!(tree.attempt_turn_ceiling(&child), Some(3));
    let caller_path = tree.node("root").unwrap().path.clone();
    let hook = tree
        .mint_hook(&child, "root", &caller_path, &mint_req(), None)
        .unwrap();
    assert_eq!(hook.command, "echo one");
    assert!(Arc::ptr_eq(&tree.cell_budget(&child).unwrap(), &budget));
    assert_eq!(budget.budget(), 2);
    assert_eq!(tree.cell_budget("root").unwrap().budget(), 4);
}

/// An adopter spawned under a successor epoch that requires review evidence
/// is held to it after a further append that does not.
#[test]
fn an_adopter_on_a_successor_epoch_keeps_its_review_evidence_rule() {
    let repo = git_repo();
    let mut tree = tree_at(
        repo.path().to_path_buf(),
        crate::protocol::WriterMode::SingleLease,
    );
    let first = successor_of(&tree, |epoch| {
        epoch.require_review_evidence = true;
        epoch.max_depth = 2;
        epoch
            .roster
            .get_mut(&template("orchestrate"))
            .unwrap()
            .max_tier = crate::protocol::Tier::Mutate;
    });
    assert_eq!(tree.append_epoch(first).unwrap(), 1);
    let lead = tree
        .spawn_agent("root", &lead_req(), None)
        .unwrap()
        .agent_id;
    let second = successor_of(&tree, |epoch| epoch.require_review_evidence = false);
    assert_eq!(tree.append_epoch(second).unwrap(), 2);
    let worker = tree.spawn_agent(&lead, &mint_req(), None).unwrap().agent_id;
    assert_eq!(tree.node(&worker).unwrap().epoch_no, 1);
    std::fs::write(tree.node(&worker).unwrap().path.join("W.txt"), "worker\n").unwrap();
    tree.record_turn_completed(&worker).unwrap();
    tree.settle_for_test();
    let candidate = candidate_of(&tree, &worker);

    let refused = tree
        .adopt_candidate(&lead, &adopt_req(&candidate), None)
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::EvidenceMismatch);
}

/// A roster apply on a quiescent tree whose finished child was spawned under
/// the current epoch appends: the child keeps the card it was spawned with.
/// With nothing but the root on the current epoch it replaces it in place.
#[test]
fn a_roster_apply_appends_while_a_finished_agent_keeps_the_current_epoch() {
    let (_root, mut tree) = tree();
    let in_place = successor_of(&tree, |epoch| epoch.transient_resumes = 2);
    tree.begin_successor_epoch(in_place.clone()).unwrap();
    assert_eq!(tree.epochs().current, 0, "only the root referenced epoch 0");
    assert_eq!(tree.current_epoch(), &in_place);

    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.close_agent("root", &close_req(&child), None).unwrap();
    tree.settle_for_test();
    let card = tree.epoch_of(&child).unwrap().clone();
    let applied = successor_of(&tree, |epoch| {
        epoch.roster.clear();
        epoch.working_set.clear();
    });
    tree.begin_successor_epoch(applied.clone()).unwrap();
    assert_eq!(tree.epochs().current, 1);
    assert_eq!(tree.current_epoch(), &applied);
    assert_eq!(tree.node(&child).unwrap().epoch_no, 0);
    assert_eq!(
        tree.epoch_of(&child),
        Some(&card),
        "the closed child keeps its card"
    );
}

/// The per-prompt attempt bound is the root's epoch state: a reopen derives
/// it from the current epoch instead of resetting it to the host constant.
#[test]
fn a_reopen_rederives_the_attempt_bound_from_the_current_epoch() {
    let (_root, mut tree) = tree();
    let lowered = successor_of(&tree, |epoch| epoch.max_attempts = 3);
    assert_eq!(tree.append_epoch(lowered).unwrap(), 1);
    assert_eq!(tree.max_attempts(), 3);
    let store = MemoryTreeStore::new();
    let _ = store.save_in_custody(&tree).unwrap();
    let reopened = ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
    assert_eq!(reopened.max_attempts(), 3);
}

/// A child's spawn mid-reservation binds the child's own epoch, so it does
/// not hold an append back; only a root spawn does.
#[test]
fn only_a_root_spawn_mid_reservation_blocks_an_append() {
    let (_root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let successor = successor_of(&tree, |epoch| epoch.transient_resumes = 2);
    tree.take_spawn_slot(&child);
    assert_eq!(tree.append_epoch(successor.clone()).unwrap(), 1);
    tree.release_spawn_slot(&child);
    tree.take_spawn_slot("root");
    let next = successor_of(&tree, |epoch| epoch.transient_resumes = 0);
    assert_eq!(
        tree.append_epoch(next).unwrap_err().message,
        ProductAgentTree::TREE_EPOCH_REBIND_PENDING_SPAWNS
    );
}

/// The `epoch_pending` publish-or-validate pair: the bytes are recorded
/// before the publication, settle appends exactly them and removes the row,
/// a repeat is idempotent, and a switch that did not land is withdrawn.
#[test]
fn a_pending_epoch_settles_from_its_bytes_or_is_withdrawn() {
    let (_root, mut tree) = tree();
    let live = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let successor = successor_of(&tree, |epoch| epoch.transient_resumes = 2);
    let recorded = tree.record_epoch_pending("switch-1", &successor).unwrap();
    assert_eq!(recorded.epoch_no, 1);
    assert_eq!(
        tree.record_epoch_pending("switch-1", &successor).unwrap(),
        recorded,
        "recording the same bytes again validates them"
    );
    let other = successor_of(&tree, |epoch| epoch.transient_resumes = 0);
    assert_eq!(
        tree.record_epoch_pending("switch-2", &other)
            .unwrap_err()
            .code,
        TopologyErrorCode::GrantDenied
    );
    assert_eq!(
        tree.record_epoch_pending("switch-1", &other)
            .unwrap_err()
            .code,
        TopologyErrorCode::OperationRecoveryRequired
    );

    // The row survives a store round trip, and the live child does not
    // block the settle.
    let store = MemoryTreeStore::new();
    let _ = store.save_in_custody(&tree).unwrap();
    let mut reopened = ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
    assert_eq!(reopened.epoch_pending().unwrap(), vec![recorded]);
    assert!(!reopened.node(&live).unwrap().status.is_finished());
    assert_eq!(reopened.settle_epoch_pending("switch-1").unwrap(), Some(1));
    assert_eq!(reopened.current_epoch(), &successor);
    assert_eq!(reopened.node(&live).unwrap().epoch_no, 0);
    assert_eq!(reopened.epoch_pending().unwrap(), Vec::new());
    assert_eq!(reopened.settle_epoch_pending("switch-1").unwrap(), None);

    let third = successor_of(&reopened, |epoch| epoch.transient_resumes = 1);
    reopened.record_epoch_pending("switch-3", &third).unwrap();
    assert_eq!(
        reopened
            .withdraw_epoch_pending_except(Some("switch-4"))
            .unwrap(),
        ["switch-3"]
    );
    assert_eq!(reopened.epoch_pending().unwrap(), Vec::new());
    assert_eq!(reopened.epochs().current, 1);
    assert!(!reopened.withdraw_epoch_pending("switch-3"));
}

/// A pending epoch installs only as the tree's next epoch: once another
/// append took that number, its settle refuses and leaves the tree and the
/// row as they were.
#[test]
fn a_pending_epoch_numbered_for_another_epoch_does_not_settle() {
    let (_root, mut tree) = tree();
    let pending = successor_of(&tree, |epoch| epoch.transient_resumes = 2);
    assert_eq!(
        tree.record_epoch_pending("switch-1", &pending)
            .unwrap()
            .epoch_no,
        1
    );
    let other = successor_of(&tree, |epoch| epoch.transient_resumes = 0);
    assert_eq!(tree.append_epoch(other.clone()).unwrap(), 1);
    assert_eq!(
        tree.settle_epoch_pending("switch-1").unwrap_err().code,
        TopologyErrorCode::OperationRecoveryRequired
    );
    assert_eq!(tree.epochs().current, 1);
    assert_eq!(tree.current_epoch(), &other);
    assert_eq!(tree.epoch_pending().unwrap().len(), 1);
}

/// A rebind that keeps the roster moves only the tree's view of the root
/// binding: no epoch, no quiescence, the admission state kept.
#[test]
fn a_root_bind_view_moves_with_live_descendants_and_no_epoch() {
    let (_root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let epoch = tree.current_epoch().clone();
    let state = tree.state;
    tree.record_root_bind("claude", "claude-next", Some("high"));
    let root = tree.node("root").unwrap();
    assert_eq!(
        (
            root.account.as_str(),
            root.tuple_id.as_str(),
            root.effort.as_deref()
        ),
        ("claude", "claude-next", Some("high"))
    );
    assert_eq!(tree.selected_tuple, "claude-next");
    assert_eq!(tree.current_epoch(), &epoch);
    assert_eq!(tree.epochs(), &TreeEpochs::default());
    assert_eq!(tree.state, state);
    assert!(!tree.node(&child).unwrap().status.is_finished());
}
