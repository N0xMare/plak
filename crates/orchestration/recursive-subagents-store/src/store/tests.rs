#[cfg(feature = "commit-trace")]
#[test]
fn commit_trace_frameless_tree_helpers_release_for_another_thread()
-> Result<(), Box<dyn std::error::Error>> {
    use plakous_store::commit_trace::{KILL_ENV, TRACE_ENV};
    let root = tempfile::tempdir()?;
    let trace = root.path().join("trace.jsonl");
    let output = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "store::tests::commit_trace_frameless_tree_child",
            "--nocapture",
        ])
        .env("PLAK_SW1_TREE_FRAMELESS_ROOT", root.path())
        .env(TRACE_ENV, &trace)
        .env_remove(KILL_ENV)
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(trace)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["ordinal"], 1);
    assert_eq!(lines[1]["ordinal"], 2);
    assert!(lines.iter().all(|line| line["store"] == "tree"));
    Ok(())
}

#[cfg(feature = "commit-trace")]
#[test]
fn commit_trace_frameless_tree_child() -> Result<(), Box<dyn std::error::Error>> {
    use super::{SqliteTreeStore, TreeStoreError};
    let Some(root) = std::env::var_os("PLAK_SW1_TREE_FRAMELESS_ROOT") else {
        return Ok(());
    };
    let path = std::path::PathBuf::from(root).join("tree.sqlite3");
    let store = SqliteTreeStore::open(&path)?;
    let no_op = |database: &SubagentsDatabase| {
        database
            .connection
            .execute_batch("BEGIN IMMEDIATE; SELECT singleton FROM subagents_schema; COMMIT;")
            .map_err(|error| TreeStoreError::unavailable(error.to_string()))
    };
    store
        .with_writer(|writer| no_op(&writer.database))
        .map_err(|error| std::io::Error::other(error.detail))?;
    store
        .with_database(no_op)
        .map_err(|error| std::io::Error::other(error.detail))?;
    std::thread::spawn(move || -> Result<(), OpenError> {
        let peer = SqliteTreeStore::open(&path)?;
        peer.with_writer(|writer| {
            writer
                .database
                .connection
                .execute("UPDATE subagents_schema SET instance_id=instance_id", [])
                .map(|_| ())
                .map_err(|error| TreeStoreError::unavailable(error.to_string()))
        })
        .map_err(|error| OpenError::unavailable_text(error.detail))
    })
    .join()
    .map_err(|_| std::io::Error::other("peer tree panicked"))??;
    Ok(())
}
use super::*;

#[test]
#[allow(clippy::unwrap_used, reason = "isolated SQLite inspection fixture")]
fn inspection_distinguishes_missing_store_and_tree_without_creating_state() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("subagents.sqlite3");
    assert!(matches!(
        SqliteTreeStore::inspect(&database, "root").unwrap(),
        TreeInspection::StoreAbsent
    ));
    assert!(!database.exists());
    let absent_parent = root.path().join("missing").join("subagents.sqlite3");
    assert!(matches!(
        SqliteTreeStore::inspect(&absent_parent, "root").unwrap(),
        TreeInspection::StoreAbsent
    ));
    assert!(!absent_parent.parent().unwrap().exists());

    let store = SqliteTreeStore::open(&database).unwrap();
    let before: (i64, String) = store
        .with_database(|database| {
            database
                .connection
                .query_row(
                    "SELECT (SELECT count(*) FROM agent_trees), fingerprint FROM subagents_schema WHERE singleton=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| TreeStoreError::unavailable("could not inspect test snapshot"))
        })
        .unwrap();
    assert!(matches!(
        SqliteTreeStore::inspect(&database, "root").unwrap(),
        TreeInspection::TreeAbsent
    ));
    let after: (i64, String) = store
        .with_database(|database| {
            database
                .connection
                .query_row(
                    "SELECT (SELECT count(*) FROM agent_trees), fingerprint FROM subagents_schema WHERE singleton=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| TreeStoreError::unavailable("could not inspect test snapshot"))
        })
        .unwrap();
    assert_eq!(after, before);
}

#[test]
fn save_failure_identifies_sqlite_cause_without_backend_text() {
    use plakous_recursive_subagents::tree::TreeStoreErrorKind;
    for (code, kind, detail) in [
        (
            rusqlite::ffi::SQLITE_FULL,
            TreeStoreErrorKind::StorageFull,
            "agent tree storage is full",
        ),
        (
            rusqlite::ffi::SQLITE_BUSY,
            TreeStoreErrorKind::Unavailable,
            "could not durably commit agent tree: database_busy",
        ),
    ] {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("private database path and SQL".to_owned()),
        );
        let mapped = sqlite_unavailable("could not durably commit agent tree", &error);
        assert_eq!(mapped.kind, kind);
        assert_eq!(mapped.detail, detail);
        assert!(!mapped.detail.contains("private database path"));
    }
}

/// Only an acknowledged commit leaves an image behind: every refusal,
/// failure and delete falls the next save back to the whole-tree path.
#[cfg(feature = "test-support")]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod images {
    use std::sync::Arc;

    use plakous_recursive_subagents::protocol::{TopologyGrant, WriterMode};
    use plakous_recursive_subagents::tree::{
        FakeVcs, PersistedTree, ProductAgentTree, TreeStore, TreeStoreErrorKind,
    };

    use super::super::SqliteTreeStore;

    struct Fixture {
        _root: tempfile::TempDir,
        store: SqliteTreeStore,
        tree: ProductAgentTree,
    }

    /// A tree saved twice, so its image exists and the second save took
    /// the row-level path.
    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let store = SqliteTreeStore::open(&root.path().join("subagents.sqlite3")).unwrap();
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            Arc::new(FakeVcs::new()),
            "tree",
            "root",
            "owner",
            root.path().to_owned(),
            "tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        for _ in 0..2 {
            tree.revision += 1;
            let revision = store.save(&tree).unwrap();
            tree.acknowledge_revision(revision);
        }
        assert_eq!(images(&store), 1);
        Fixture {
            _root: root,
            store,
            tree,
        }
    }

    fn images(store: &SqliteTreeStore) -> usize {
        store.writer.lock().unwrap().images.len()
    }

    #[test]
    fn a_stale_refusal_drops_the_image() {
        let Fixture { store, tree, .. } = fixture();
        let mut stale = tree.clone();
        stale.stored_revision += 1;
        stale.revision += 2;
        let error = store.save(&stale).unwrap_err();
        assert!(matches!(
            error.kind,
            TreeStoreErrorKind::StaleRevision { .. }
        ));
        assert_eq!(images(&store), 0);
    }

    #[test]
    fn a_foreign_key_refusal_drops_the_image() {
        let Fixture { store, tree, .. } = fixture();
        let mut grants = tree.grants().clone();
        grants.insert(
            "ghost".to_owned(),
            TopologyGrant {
                max_nodes: 1,
                max_children: 1,
                ..TopologyGrant::default()
            },
        );
        let mut broken = PersistedTree::from_rows(
            tree.tree_id.clone(),
            tree.root_agent_id.clone(),
            tree.revision + 1,
            tree.host_generation,
            tree.state,
            tree.writer_mode,
            tree.selected_tuple.clone(),
            tree.launch_checkout.clone(),
            tree.snapshot.clone(),
            tree.nodes().clone(),
            grants,
            tree.attempts().clone(),
            tree.candidates().clone(),
            tree.workspaces().clone(),
            tree.messages().clone(),
            tree.operations().clone(),
        )
        .with_feed_rows(tree.commands().clone(), tree.feed_cursors().clone())
        .rows()
        .clone();
        broken.stored_revision = tree.revision;
        let error = store.save(&broken).unwrap_err();
        assert_eq!(error.kind, TreeStoreErrorKind::Unavailable, "{error}");
        assert_eq!(images(&store), 0);
    }

    #[test]
    fn a_failed_commit_drops_the_image() {
        let Fixture {
            store, mut tree, ..
        } = fixture();
        store
            .writer
            .lock()
            .unwrap()
            .database
            .connection
            .commit_hook(Some(|| true))
            .unwrap();
        tree.revision += 1;
        let error = store.save(&tree).unwrap_err();
        assert_eq!(
            error.detail,
            "could not durably commit agent tree: other_sqlite_failure"
        );
        assert_eq!(images(&store), 0);
        store
            .writer
            .lock()
            .unwrap()
            .database
            .connection
            .commit_hook(None::<fn() -> bool>)
            .unwrap();
        let revision = store.save(&tree).unwrap();
        tree.acknowledge_revision(revision);
        assert_eq!(images(&store), 1);
    }

    #[test]
    fn a_delete_drops_the_image() {
        let Fixture { store, tree, .. } = fixture();
        store.delete(&tree.tree_id, tree.revision).unwrap();
        assert_eq!(images(&store), 0);
        assert!(!store.exists(&tree.tree_id).unwrap());
    }
}
