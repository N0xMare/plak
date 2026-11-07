//! Cache coherence across trees, grants and handles, measured at SQLite's WAL.

#![allow(clippy::unwrap_used, reason = "deterministic store regressions")]

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous_recursive_subagents::protocol::{NodeStatus, WriterMode};
use plakous_recursive_subagents::tree::{CustodyHost, FakeVcs, HostInstance};
use rusqlite::hooks::{Action, AuthAction, AuthContext, Authorization};

use super::*;

thread_local! {
    static COMMITS: Cell<usize> = const { Cell::new(0) };
}

struct Fixture {
    root: tempfile::TempDir,
    store: SqliteTreeStore,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = SqliteTreeStore::open(&root.path().join("trees.sqlite3")).unwrap();
        Self { root, store }
    }

    fn path(&self) -> std::path::PathBuf {
        self.root.path().join("trees.sqlite3")
    }

    fn tree(&self, id: &str) -> ProductAgentTree {
        ProductAgentTree::bootstrap_root_for_test_with(
            Arc::new(FakeVcs::new()),
            id,
            "root",
            "owner",
            self.root.path().to_owned(),
            "tuple",
            WriterMode::SingleLease,
        )
        .unwrap()
    }

    fn take(&self, id: &str) -> CustodyClaim {
        self.store
            .take_custody(
                id,
                &CustodyHost {
                    host_id: "host".into(),
                    instance: HostInstance::parse("500-0000000000000001").unwrap(),
                    binary: None,
                    host_name: "machine".into(),
                },
                None,
            )
            .unwrap()
    }

    fn save(&self, tree: &mut ProductAgentTree, grant: &CustodyClaim) {
        let _ = self.store.save(tree, grant).unwrap();
        tree.stored_revision = tree.revision;
    }

    fn observe(&self) -> Arc<AtomicUsize> {
        let deleted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&deleted);
        self.store
            .with_connection(|connection| {
                connection.wal_hook(Some(|_, _| {
                    COMMITS.with(|count| count.set(count.get() + 1));
                    Ok(())
                }));
                connection
                    .update_hook(Some(move |action, _: &str, table: &str, _| {
                        if action == Action::SQLITE_DELETE && table == "agent_nodes" {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    }))
                    .unwrap();
                Ok::<_, TreeStoreError>(())
            })
            .unwrap();
        COMMITS.with(|count| count.set(0));
        deleted
    }

    fn status(&self, id: &str) -> NodeStatus {
        let TreeInspection::Tree(tree) = SqliteTreeStore::inspect(&self.path(), id).unwrap() else {
            panic!("the tree is absent");
        };
        tree.rows().nodes()["root"].status
    }
}

fn commits() -> usize {
    COMMITS.with(Cell::get)
}

// A separately connected, fenced writer, as another process would use.
fn foreign_save(
    connection: &mut rusqlite::Connection,
    tree: &ProductAgentTree,
    grant: &CustodyClaim,
) {
    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    crate::custody::prove_in(&tx, &tree.tree_id, grant).unwrap();
    fence_revision(
        stored_revision(&tx, &tree.tree_id).unwrap(),
        tree.stored_revision,
        tree.revision,
    )
    .unwrap();
    delete_agent_tree_rows(&tx, &tree.tree_id).unwrap();
    insert_agent_tree_rows(&tx, tree).unwrap();
    tx.commit().unwrap();
}

#[test]
fn alternating_resident_trees_commit_once_per_change_and_never_for_quiet_saves() {
    let f = Fixture::new();
    let mut a = f.tree("a");
    let mut b = f.tree("b");
    let ga = f.take("a");
    let gb = f.take("b");
    f.save(&mut a, &ga);
    f.save(&mut b, &gb);
    let peer = SqliteTreeStore::open(&f.path()).unwrap();
    let deleted = f.observe();
    for ordinal in 1..=16 {
        a.host_generation += 1;
        a.revision += 1;
        f.save(&mut a, &ga);
        assert_eq!(commits(), ordinal * 2 - 1);
        let _ = peer.save(&b, &gb).unwrap();
        assert_eq!(commits(), ordinal * 2 - 1, "quiet B must not commit");
        b.host_generation += 1; // A same-revision receipt under the same grant.
        let _ = peer.save(&b, &gb).unwrap();
        assert_eq!(commits(), ordinal * 2);
        f.save(&mut a, &ga);
        assert_eq!(commits(), ordinal * 2, "quiet A must not commit");
    }
    assert_eq!(deleted.load(Ordering::Relaxed), 0);
}

#[test]
fn another_trees_same_revision_same_grant_write_keeps_the_cached_baseline() {
    let f = Fixture::new();
    let mut a = f.tree("a");
    let mut b = f.tree("b");
    let ga = f.take("a");
    let gb = f.take("b");
    f.save(&mut a, &ga);
    f.save(&mut b, &gb);
    let deleted = f.observe();
    let mut foreign = rusqlite::Connection::open(f.path()).unwrap();
    b.host_generation += 1;
    foreign_save(&mut foreign, &b, &gb);
    f.store
        .with_connection(|connection| {
            connection
                .authorizer(Some(|context: AuthContext<'_>| {
                    if matches!(
                        context.action,
                        AuthAction::Read {
                            table_name: "agent_nodes",
                            ..
                        }
                    ) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
            Ok::<_, TreeStoreError>(())
        })
        .unwrap();
    f.save(&mut a, &ga);
    assert_eq!(commits(), 0);
    assert_eq!(deleted.load(Ordering::Relaxed), 0);
}

#[test]
fn same_grant_handles_share_same_revision_receipts() {
    let f = Fixture::new();
    let mut tree = f.tree("a");
    let grant = f.take("a");
    f.save(&mut tree, &grant);
    let peer = SqliteTreeStore::open(&f.path()).unwrap();
    let mut receipt = tree.clone();
    receipt.set_node_status_for_test("root", NodeStatus::Interrupted);
    let _ = peer.save(&receipt, &grant).unwrap();
    f.save(&mut tree, &grant);
    assert_eq!(f.status("a"), NodeStatus::Running);
}

#[test]
fn break_glass_then_a_fresh_grant_rebuilds_without_deleting_unchanged_rows() {
    let f = Fixture::new();
    let mut tree = f.tree("a");
    let old = f.take("a");
    f.save(&mut tree, &old);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "store::cache_tests::foreign_break_glass_helper",
        ])
        .env("PLAK_TREE_CACHE_TEST_DATABASE", f.path())
        .status()
        .unwrap();
    assert!(status.success());
    let new = f.take("a");
    assert_ne!(new.fence, old.fence);
    let deleted = f.observe();
    f.save(&mut tree, &new);
    assert_eq!(f.status("a"), NodeStatus::Running);
    assert_eq!(commits(), 1);
    assert_eq!(deleted.load(Ordering::Relaxed), 0);
    assert!(f.store.save(&tree, &old).is_err());
}

#[test]
#[ignore = "subprocess of the break-glass cache regression"]
fn foreign_break_glass_helper() {
    let path = std::env::var_os("PLAK_TREE_CACHE_TEST_DATABASE").unwrap();
    let store = SqliteTreeStore::open(Path::new(&path)).unwrap();
    let record = store.break_custody("a").unwrap().unwrap();
    let grant = store.take_custody("a", &record.holding.host, None).unwrap();
    let mut tree = store.load("a").unwrap().unwrap().rows().clone();
    tree.set_node_status_for_test("root", NodeStatus::Interrupted);
    let _ = store.save(&tree, &grant).unwrap();
}

#[test]
fn a_cold_reopen_rebuilds_only_deltas() {
    let f = Fixture::new();
    let mut tree = f.tree("a");
    let grant = f.take("a");
    f.save(&mut tree, &grant);
    let Fixture { root, store } = f;
    drop(store);
    let store = SqliteTreeStore::open(&root.path().join("trees.sqlite3")).unwrap();
    let f = Fixture { root, store };
    let deleted = f.observe();
    tree.host_generation += 1;
    f.save(&mut tree, &grant);
    assert_eq!(commits(), 1);
    assert_eq!(deleted.load(Ordering::Relaxed), 0);
}

#[test]
fn a_changed_revision_on_the_same_connection_rebuilds_the_baseline() {
    let f = Fixture::new();
    let mut tree = f.tree("a");
    let grant = f.take("a");
    f.save(&mut tree, &grant);
    let mut receipt = tree.clone();
    receipt.revision += 1;
    receipt.set_node_status_for_test("root", NodeStatus::Interrupted);
    f.store
        .with_connection(|connection| {
            foreign_save(connection, &receipt, &grant);
            Ok::<_, TreeStoreError>(())
        })
        .unwrap();
    tree.stored_revision = receipt.revision;
    tree.revision = receipt.revision + 1;
    f.save(&mut tree, &grant);
    assert_eq!(f.status("a"), NodeStatus::Running);
}

#[test]
fn load_refreshes_the_baseline_even_at_the_same_revision_and_grant() {
    let f = Fixture::new();
    let mut tree = f.tree("a");
    let grant = f.take("a");
    f.save(&mut tree, &grant);
    let mut receipt = tree.clone();
    receipt.set_node_status_for_test("root", NodeStatus::Interrupted);
    let mut foreign = rusqlite::Connection::open(f.path()).unwrap();
    foreign_save(&mut foreign, &receipt, &grant);
    let loaded = f.store.load("a").unwrap().unwrap();
    assert_eq!(
        loaded.rows().nodes()["root"].status,
        NodeStatus::Interrupted
    );
    f.save(&mut tree, &grant);
    assert_eq!(f.status("a"), NodeStatus::Running);
}
