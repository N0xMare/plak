use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use plakous_host_protocol::selection::CompositionSelection;

use super::{ForkedFrom, NodeLineId, NodePlacement, NodeWorkspace, TreeEpochs};
use crate::tree::test_support::{spawn_req, tree};
use crate::tree::{MemoryTreeStore, ProductAgentTree, TreeStore};

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
