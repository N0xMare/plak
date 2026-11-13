#![cfg(feature = "test-support")]
#![allow(
    clippy::unwrap_used,
    reason = "deterministic ownership-move conformance fixtures"
)]

use plakous_recursive_subagents::protocol::WriterMode;
use plakous_recursive_subagents::tree::host_move::DormantTree;
use plakous_recursive_subagents::tree::{
    AgentCommand, AgentCommandKind, CustodyHolding, CustodyHost, CustodyRecord, FakeVcs,
    HostInstance, MemoryTreeStore, ProductAgentTree, TreeStore,
};
use plakous_recursive_subagents_store::SqliteTreeStore;
use std::sync::Arc;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";

fn host(instance: &str) -> CustodyHost {
    CustodyHost {
        host_id: "move-owner".into(),
        instance: HostInstance::parse(instance).unwrap(),
        binary: Some("build".into()),
        host_name: "machine".into(),
    }
}

fn seed(root: &std::path::Path) -> DormantTree {
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::new(FakeVcs::new()),
        "move-tree",
        "root",
        "owner",
        root.to_owned(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    tree.seed_feed_cursor(SESSION, 20);
    tree.record_command(AgentCommand::intent(
        "owed",
        "root",
        None,
        AgentCommandKind::RootBatch,
        "next",
        vec![],
    ))
    .unwrap();
    let cut = DormantTree::capture(&tree).unwrap();
    let rows = cut.reconstruct(&std::collections::BTreeMap::new()).unwrap();
    DormantTree::capture(rows.rows()).unwrap()
}

fn check_install(store: &dyn TreeStore, cut: &DormantTree) {
    let original = host("700-0000000000000001");
    let source = CustodyRecord {
        fence: 17,
        holding: CustodyHolding {
            host: original,
            claimed_at_ms: 1,
            renewed_at_ms: 1,
        },
        released: false,
    };
    let digest = "a".repeat(64);
    store
        .install_move_seed("move", &digest, cut, &source)
        .unwrap();
    let installed = store.load(cut.tree_id()).unwrap().unwrap();
    assert_eq!(installed.rows().feed_cursor(SESSION), 20);
    let claimant = host("701-0000000000000002");
    let claim = store
        .custody_port()
        .take_custody(cut.tree_id(), &claimant, None)
        .unwrap();
    assert_eq!(claim.fence, 18);
    let mut opened = ProductAgentTree::reopen(installed);
    opened
        .record_command(AgentCommand::intent(
            "later",
            "root",
            None,
            AgentCommandKind::RootBatch,
            "new input",
            vec![],
        ))
        .unwrap();
    let published = store.save(&opened, &claim).unwrap();
    opened.acknowledge_revision(published);
    store
        .install_move_seed("move", &digest, cut, &source)
        .unwrap();
    assert!(
        store
            .install_move_seed("other", &digest, cut, &source)
            .is_err()
    );
    let mut reopened = ProductAgentTree::reopen(store.load(cut.tree_id()).unwrap().unwrap());
    assert!(reopened.command("later").is_some());
    reopened.clear_feed_views();
    assert_eq!(reopened.feed_cursor(SESSION), 20);
    assert!(reopened.command("owed").is_some());
    assert_eq!(
        store
            .custody_port()
            .custody(cut.tree_id())
            .unwrap()
            .unwrap()
            .fence,
        18
    );
}

#[test]
fn dormant_tree_memory_install_keeps_its_semantic_cut_and_custody_floor() {
    let root = tempfile::tempdir().unwrap();
    check_install(&MemoryTreeStore::new(), &seed(root.path()));
}

#[test]
fn dormant_tree_sqlite_install_keeps_its_semantic_cut_and_custody_floor_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let cut = seed(root.path());
    check_install(&SqliteTreeStore::open(&path).unwrap(), &cut);
    let store = SqliteTreeStore::open(&path).unwrap();
    let mut reopened = ProductAgentTree::reopen(store.load(cut.tree_id()).unwrap().unwrap());
    assert!(reopened.command("later").is_some());
    reopened.clear_feed_views();
    assert_eq!(reopened.feed_cursor(SESSION), 20);
    assert!(reopened.command("owed").is_some());
}

#[test]
fn dormant_tree_source_inspection_reads_rows_and_custody_on_one_snapshot_without_a_take() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("missing").join("subagents.sqlite3");
    assert!(
        SqliteTreeStore::inspect_move_tree(&absent, "move-tree")
            .unwrap()
            .is_none()
    );
    assert!(!absent.parent().unwrap().exists());
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let cut = seed(root.path());
    check_install(&store, &cut);
    let before = std::fs::read(&path).unwrap();
    let custody = store
        .custody_port()
        .custody(cut.tree_id())
        .unwrap()
        .unwrap();
    let (observed, floor) = SqliteTreeStore::inspect_move_tree(&path, cut.tree_id())
        .unwrap()
        .unwrap();
    assert_eq!(floor, custody);
    assert_eq!(floor.fence, 18);
    assert!(!floor.released);
    assert_eq!(
        DormantTree::capture(observed.rows())
            .unwrap()
            .digest()
            .unwrap(),
        DormantTree::capture(store.load(cut.tree_id()).unwrap().unwrap().rows())
            .unwrap()
            .digest()
            .unwrap()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        store
            .custody_port()
            .custody(cut.tree_id())
            .unwrap()
            .unwrap(),
        custody
    );
}
