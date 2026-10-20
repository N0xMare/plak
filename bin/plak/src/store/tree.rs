//! The product's adapter onto the agent tree's own store.
//!
//! The tree's rows live in
//! `subagents.sqlite3`, with their own schema version and their own DDL
//! fingerprint, and `plakous-recursive-subagents-store` owns the file and the
//! codecs. What is left here is the product opening it by `PlakPaths`, and the
//! `String`-typed convenience the CLI flows read.

use std::sync::Arc;

use plakous_recursive_subagents::tree::{ProductAgentTree, TreeStore};
use plakous_recursive_subagents_store::SqliteTreeStore;

use crate::home::paths::PlakPaths;
use plakous_hosted::{CliFailure, CliResult};

/// The tree store for this home, opened on its own file.
pub fn open_tree_store(paths: &PlakPaths) -> Result<SqliteTreeStore, String> {
    paths.ensure_layout()?;
    SqliteTreeStore::open(&paths.subagents_database()).map_err(|error| error.message().to_owned())
}

/// The tree store as the port, for a caller that holds it for a runtime's
/// life.
pub fn tree_store(paths: &PlakPaths) -> Result<Arc<dyn TreeStore>, String> {
    Ok(Arc::new(open_tree_store(paths)?))
}

/// Every persisted tree id. A headless `plak config set` fences a roster or
/// orchestration edit against all of them, because no session names one.
pub fn tree_ids(paths: &PlakPaths) -> Result<Vec<String>, String> {
    open_tree_store(paths)?
        .ids()
        .map_err(|error| error.detail.clone())
}

/// Whether the tree is persisted at all.
pub fn tree_exists(paths: &PlakPaths, tree_id: &str) -> Result<bool, String> {
    open_tree_store(paths)?
        .exists(tree_id)
        .map_err(|error| error.detail.clone())
}

/// The tree's rows with the reopen law applied.
pub fn load_tree(paths: &PlakPaths, tree_id: &str) -> Result<Option<ProductAgentTree>, String> {
    Ok(open_tree_store(paths)?
        .load(tree_id)
        .map_err(|error| error.detail.clone())?
        .map(ProductAgentTree::reopen))
}

/// A whole-tree publish through the port's compare-and-swap.
pub fn save_tree(paths: &PlakPaths, tree: &mut ProductAgentTree) -> Result<(), String> {
    let acknowledged = open_tree_store(paths)?
        .save(tree)
        .map_err(|error| error.detail.clone())?;
    tree.acknowledge_revision(acknowledged);
    Ok(())
}

/// Plak's tree store: `subagents.sqlite3` under one product home.
///
/// The product side of `agent::TreeStoreFactory`; the roots hold the port and
/// never learn where the file is.
pub(crate) struct ProductTreeStoreFactory {
    paths: PlakPaths,
}

impl ProductTreeStoreFactory {
    pub(crate) const fn new(paths: PlakPaths) -> Self {
        Self { paths }
    }
}

impl plakous_hosted::agent::TreeStoreFactory for ProductTreeStoreFactory {
    fn open(&self) -> CliResult<Arc<dyn TreeStore>> {
        tree_store(&self.paths).map_err(CliFailure::new)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::*;
    use std::path::Path;

    use plakous_hosted::agent_tree::TreeAdmissionState;

    use tempfile::tempdir;

    /// Pins a read-only `review` template on `account` so
    /// `SpawnAgentRequest::test_review` resolves, under the product's roster.
    fn pin_review(tree: &mut ProductAgentTree, account: &str, tuple: &str, effort: Option<&str>) {
        tree.pin_bind_hints_for_test(crate::roster_input::product_bind_hints());
        let mut binding = plakous_hosted::orchestration::RosterBinding::leaf(account, tuple);
        binding.effort = effort.map(str::to_owned);
        binding.max_tier = plakous_hosted::agent_tree_protocol::Tier::ReadOnly;
        tree.pin_template("review", binding);
    }

    #[test]
    fn pending_workspace_discard_survives_sqlite_reopen_without_removal_replay() {
        use plakous_hosted::agent_tree_protocol::{
            NodeStatus, SpawnAgentRequest, Tier, WorkspaceIdRequest, WorkspaceStatus, WriterMode,
        };
        use plakous_hosted::occupancy_vcs::fake::{FakeOp, FakeVcs};
        let root = tempdir().unwrap();
        let database = root.path().join("pending-discard.sqlite3");
        let store = SqliteTreeStore::open(&database).unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "pending-discard",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "implement", "work")
                    .with_fork(),
                None,
            )
            .unwrap();
        vcs.seal_next_with(Ok(FakeVcs::base_for(&child.agent_id)));
        // Finished, not closed: a closed copy would already be pruned.
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let request = WorkspaceIdRequest {
            workspace_id: child.workspace.id.clone(),
        };
        tree.discard_workspace("root", &request, Some("remove-once"))
            .unwrap();
        tree.acknowledge_revision(store.save(&tree).unwrap());
        let effects = tree.take_effects();
        assert!(tree.operations()["remove-once"].terminal_response.is_none());
        drop(store);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut reopened =
            ProductAgentTree::reopen(store.load("pending-discard").unwrap().unwrap());
        assert_eq!(
            reopened.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Discarding
        );
        assert_eq!(
            reopened.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(
            reopened
                .discard_workspace("root", &request, Some("remove-once"))
                .is_err()
        );
        assert!(reopened.take_effects().is_empty());
        assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
        // Only the original retained ticket can finish the removal.
        tree.complete(effects.run(vcs.as_ref()));
        tree.acknowledge_revision(store.save(&tree).unwrap());
        let completed = store.load("pending-discard").unwrap().unwrap();
        assert_eq!(
            completed
                .rows()
                .workspace(&child.workspace.id)
                .unwrap()
                .status,
            WorkspaceStatus::Discarded
        );
        assert!(
            completed.rows().operations()["remove-once"]
                .terminal_response
                .is_some()
        );
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
    }

    #[test]
    fn keyed_spawn_identity_survives_sqlite_close_and_reopen() {
        use plakous_hosted::agent_tree_protocol::{
            SpawnAgentRequest, TopologyErrorCode, WriterMode,
        };
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("keyed"));
        let mut tree = ProductAgentTree::bootstrap_root(
            "identity-tree",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        pin_review(&mut tree, "codex", "selected-tuple", None);
        let mut request = SpawnAgentRequest::test_review();
        request.request_key = Some("durable-key".into());
        let first = tree.spawn_agent("root", &request, None).unwrap();
        let identity = tree
            .attempt(&first.agent_id)
            .unwrap()
            .spawn_identity
            .clone();
        save_tree(&paths, &mut tree).unwrap();
        drop(tree);
        let mut loaded = load_tree(&paths, "identity-tree").unwrap().unwrap();
        assert_eq!(
            loaded.attempt(&first.agent_id).unwrap().spawn_identity,
            identity
        );
        loaded.resume_tree().unwrap();
        assert_eq!(
            loaded.spawn_agent("root", &request, None).unwrap().agent_id,
            first.agent_id
        );
        request.prompt.push_str(" different");
        assert_eq!(
            loaded.spawn_agent("root", &request, None).unwrap_err().code,
            TopologyErrorCode::RequestKeyConflict
        );
        assert_eq!(loaded.nodes().len(), 2);
    }

    #[test]
    fn agent_tree_save_load_round_trips_and_reopens_paused() {
        use plakous_hosted::agent_tree::{ProductAgentTree, TreeAdmissionState};
        use plakous_hosted::agent_tree_protocol::{
            SpawnAgentRequest, TopologyErrorCode, WriterMode,
        };

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut tree = ProductAgentTree::bootstrap_root(
            "tree1",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        pin_review(&mut tree, "codex", "selected-tuple", None);
        let spawn_request = SpawnAgentRequest::test_review();
        let child = tree.spawn_agent("root", &spawn_request, None).unwrap();
        let saved_generation = tree.host_generation;
        save_tree(&paths, &mut tree).unwrap();

        let mut loaded = load_tree(&paths, "tree1").unwrap().expect("persisted tree");
        assert_eq!(loaded.state, TreeAdmissionState::RestoredPaused);
        assert_eq!(loaded.host_generation, saved_generation.saturating_add(1));
        assert_eq!(
            loaded
                .spawn_agent("root", &spawn_request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::TreeRestoredPaused
        );
        loaded.resume_tree().unwrap();
        loaded.spawn_agent("root", &spawn_request, None).unwrap();
        assert_eq!(loaded.nodes().len(), 3);
        assert_eq!(loaded.grants().len(), 3);
        assert_eq!(loaded.workspaces().len(), 1);
        assert_eq!(loaded.node("root").unwrap().description, "owner");
        assert_eq!(
            loaded.node(&child.agent_id).unwrap().description,
            "reviewer"
        );
        assert!(loaded.grants().contains_key("root"));
        assert!(loaded.grants().contains_key(&child.agent_id));
        assert_eq!(
            loaded.node(&child.agent_id).unwrap().workspace_id,
            loaded.node("root").unwrap().workspace_id
        );
        assert_eq!(loaded.writer_mode, WriterMode::SingleLease);
        // The attempt row is the unit: it comes back keyed by id
        // and the node still points at it.
        let attempt = loaded.attempt(&child.agent_id).expect("persisted attempt");
        assert_eq!(attempt.attempt_id, child.attempt_id);
        assert_eq!(loaded.attempts().len(), 2, "one per spawned child");
        assert!(loaded.grants().get("root").unwrap().may_adopt);
        assert!(!loaded.grants().get(&child.agent_id).unwrap().may_adopt);
    }

    /// Deleting a tree removes every row under the inspected revision, refuses
    /// a revision that moved, and is a no-op once the tree is gone.
    #[test]
    fn a_tree_deletes_under_its_inspected_revision_and_only_once() {
        use plakous_hosted::agent_tree::{ProductAgentTree, TreeStoreErrorKind};
        use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut tree = ProductAgentTree::bootstrap_root(
            "doomed",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        pin_review(&mut tree, "codex", "selected-tuple", None);
        tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        save_tree(&paths, &mut tree).unwrap();
        let store = open_tree_store(&paths).unwrap();
        let revision = tree.stored_revision;
        let stale = store.delete("doomed", revision + 1).unwrap_err();
        assert!(matches!(
            stale.kind,
            TreeStoreErrorKind::StaleRevision { .. }
        ));
        assert!(tree_exists(&paths, "doomed").unwrap());
        store.delete("doomed", revision).unwrap();
        assert!(!tree_exists(&paths, "doomed").unwrap());
        assert!(load_tree(&paths, "doomed").unwrap().is_none());
        store.delete("doomed", revision).unwrap();
    }

    /// The `TreeStore` conformance suite: every
    /// implementation publishes whole trees as a compare-and-swap on the
    /// revision the handle last acknowledged, keeps trees apart, hands raw
    /// rows back for the tree's own reopen law, and keeps the operation
    /// ledger replayable across a reopen. Run over the in-memory store and
    /// the SQLite store alike.
    fn tree_store_conformance(store: &dyn TreeStore, root: &Path) {
        use plakous_hosted::agent_tree::{ProductAgentTree, TreeStoreErrorKind};
        use plakous_hosted::agent_tree_protocol::{
            AttemptScopedRequest, NodeStatus, SpawnAgentRequest, SpawnWorkspace, Tier,
            TopologyErrorCode, WriterMode,
        };
        use plakous_hosted::occupancy_vcs::fake::FakeVcs;

        let fingerprint = |tree: &ProductAgentTree| {
            format!(
                "{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
                tree.tree_id,
                tree.revision,
                tree.host_generation,
                tree.state,
                tree.snapshot,
                tree.nodes(),
                tree.grants(),
                tree.attempts(),
                tree.candidates(),
                tree.workspaces(),
                tree.operations(),
            )
        };

        assert!(!store.exists("tree-conf").unwrap());
        assert_eq!(store.ids().unwrap(), [] as [String; 0]);
        assert!(store.load("tree-conf").unwrap().is_none());

        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            "tree-conf",
            "root",
            "owner",
            root.to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let request = SpawnAgentRequest::test_review();
        // The ledger: a replayable receipt survives the round trip.
        let child = tree.spawn_agent("root", &request, Some("op-1")).unwrap();
        assert_eq!(tree.stored_revision, 0, "never published");
        // The port reports the revision it published; the caller applies it.
        // A publish leaves the tree alone, so a handle that never
        // acknowledges is refused rather than silently re-fenced.
        let acknowledged = store.save(&tree).unwrap();
        assert_eq!(acknowledged.value(), tree.revision);
        assert_eq!(
            tree.stored_revision, 0,
            "the store did not edit the handle it persisted"
        );
        assert_eq!(
            store.save(&tree).unwrap_err().kind,
            TreeStoreErrorKind::StaleRevision {
                stored: tree.revision,
                offered: tree.revision
            },
            "an unacknowledged handle is refused, not quietly re-fenced"
        );
        tree.acknowledge_revision(acknowledged);
        assert_eq!(tree.stored_revision, tree.revision, "acknowledged");
        assert!(store.exists("tree-conf").unwrap());
        assert_eq!(store.ids().unwrap(), vec!["tree-conf".to_owned()]);

        // A second tree in the same store: ids come back in id order, not
        // insertion order, and one tree's publishes never touch the other.
        let mut aux = ProductAgentTree::bootstrap_root_for_test(
            "tree-aux",
            "root",
            "owner",
            root.to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        aux.acknowledge_revision(store.save(&aux).unwrap());
        assert_eq!(
            store.ids().unwrap(),
            vec!["tree-aux".to_owned(), "tree-conf".to_owned()]
        );
        let aux_fingerprint = fingerprint(&aux);

        // Raw rows come back exactly; the reopen law is not applied by the store.
        let raw = store.load("tree-conf").unwrap().expect("saved");
        assert_eq!(fingerprint(raw.rows()), fingerprint(&tree));
        assert_eq!(raw.rows().host_generation, tree.host_generation);

        // Two handles loaded at one revision: the first publish wins, the
        // second is refused by the compare-and-swap and its rows never land.
        let mut first = ProductAgentTree::reopen(store.load("tree-conf").unwrap().unwrap());
        let mut second = ProductAgentTree::reopen(store.load("tree-conf").unwrap().unwrap());
        assert_eq!(first.stored_revision, second.stored_revision);
        first.resume_tree().unwrap();
        first.spawn_agent("root", &request, None).unwrap();
        first.acknowledge_revision(store.save(&first).unwrap());
        assert_eq!(first.stored_revision, first.revision);
        second.resume_tree().unwrap();
        second.spawn_agent("root", &request, None).unwrap();
        second.spawn_agent("root", &request, None).unwrap();
        let refused = store.save(&second).unwrap_err();
        assert_eq!(
            refused.kind,
            TreeStoreErrorKind::StaleRevision {
                stored: first.revision,
                offered: second.revision
            }
        );
        assert_eq!(
            fingerprint(store.load("tree-conf").unwrap().unwrap().rows()),
            fingerprint(&first),
            "the refused publish left the winner's rows in place"
        );
        // Equal revision, different content: a handle that reached the
        // store's revision on its own is still not the handle the store
        // acknowledged.
        second.revision = first.revision;
        assert_eq!(
            store.save(&second).unwrap_err().kind,
            TreeStoreErrorKind::StaleRevision {
                stored: first.revision,
                offered: first.revision
            }
        );
        // A tree never published cannot claim an id the store already holds.
        let fresh = ProductAgentTree::bootstrap_root_for_test(
            "tree-conf",
            "root",
            "owner",
            root.to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        assert_eq!(
            store.save(&fresh).unwrap_err().kind,
            TreeStoreErrorKind::StaleRevision {
                stored: first.revision,
                offered: fresh.revision
            }
        );
        // The acknowledged handle republishing at its own revision is a
        // compare-and-swap hit; the other tree saw none of this.
        first.acknowledge_revision(store.save(&first).unwrap());
        assert_eq!(
            fingerprint(store.load("tree-aux").unwrap().unwrap().rows()),
            aux_fingerprint
        );
        // A miss stays a miss once the store holds trees, and the listing is
        // exactly what was saved.
        assert!(!store.exists("tree-none").unwrap());
        assert!(store.load("tree-none").unwrap().is_none());
        assert_eq!(
            store.ids().unwrap(),
            vec!["tree-aux".to_owned(), "tree-conf".to_owned()]
        );

        // Reopen law: a running descendant pauses the tree and bumps the
        // generation; the replayed operation returns its receipt; a fresh
        // spawn is refused until resume.
        let stored_generation = store
            .load("tree-conf")
            .unwrap()
            .unwrap()
            .rows()
            .host_generation;
        let mut reopened = ProductAgentTree::reopen(store.load("tree-conf").unwrap().unwrap());
        assert_eq!(reopened.state, TreeAdmissionState::RestoredPaused);
        assert_eq!(reopened.host_generation, stored_generation + 1);
        assert_eq!(
            reopened
                .spawn_agent("root", &request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::TreeRestoredPaused
        );
        assert_eq!(
            reopened
                .spawn_agent("root", &request, Some("op-1"))
                .unwrap(),
            child,
            "the ledger replays across a reopen"
        );
        reopened.resume_tree().unwrap();
        reopened.spawn_agent("root", &request, None).unwrap();

        // An acknowledged seal reservation, not a painted Sealing view,
        // reopens RecoveryRequired and never auto-retries.
        reopened
            .close_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: child.agent_id.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap();
        reopened.set_vcs(Arc::new(FakeVcs::new()));
        let mut fork = SpawnAgentRequest::test_named("implement", Tier::Mutate, "worker", "work");
        fork.workspace = SpawnWorkspace::Fork;
        let sealing_id = reopened.spawn_agent("root", &fork, None).unwrap().agent_id;
        assert!(reopened.begin_seal(&sealing_id));
        reopened.acknowledge_revision(store.save(&reopened).unwrap());
        let mut recovered = ProductAgentTree::reopen(store.load("tree-conf").unwrap().unwrap());
        assert_eq!(
            recovered.node(&sealing_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(
            recovered
                .fault_reason(&sealing_id)
                .is_some_and(|reason| reason.contains("in flight"))
        );
        assert!(recovered.take_effects().is_empty());
    }

    #[test]
    fn memory_tree_store_conforms() {
        use plakous_hosted::agent_tree::MemoryTreeStore;
        let root = tempdir().unwrap();
        tree_store_conformance(&MemoryTreeStore::new(), root.path());
    }

    #[test]
    fn sqlite_tree_store_conforms() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let store = open_tree_store(&paths).unwrap();
        tree_store_conformance(&store, root.path());
        // And a fresh handle on the same file sees what the port wrote.
        assert!(tree_exists(&paths, "tree-conf").unwrap());
    }

    /// Schema 19 round-trips the new rows: an attempt with every optional
    /// column set, a candidate, and the workspace base/candidate columns.
    #[test]
    fn agent_tree_attempt_and_candidate_rows_round_trip() {
        use plakous_hosted::agent_tree::{
            BlockedReason, CandidateRecord, ProductAgentTree, UsageRow,
        };
        use plakous_hosted::agent_tree_protocol::{
            AttemptId, AttemptStatus, CandidateId, SpawnAgentRequest, Tier, WorkspaceStatus,
            WriterMode,
        };

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut tree = ProductAgentTree::bootstrap_root(
            "tree-rows",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        pin_review(&mut tree, "codex", "selected-tuple", None);
        let mut request = SpawnAgentRequest::test_review();
        request.request_key = Some("key-1".into());
        let child = tree.spawn_agent("root", &request, None).unwrap();
        {
            let attempt = tree.attempt_mut_for_test(&child.agent_id);
            attempt.status = AttemptStatus::Blocked;
            attempt.ended_at = Some(42);
            attempt.retry_of = Some(AttemptId::new("c0", 3));
            attempt.candidate_id = Some(CandidateId::from_persisted("cand-c1-1".to_owned()));
            attempt.blocked = Some(BlockedReason {
                quota: "weekly".into(),
                account: "codex".into(),
                resets_at: Some(4242),
            });
            attempt.usage = UsageRow {
                elapsed_ms: 1500,
                turns: 2,
                tool_calls: 7,
                input_tokens: 1234,
                output_tokens: 56,
            };
        }
        tree.insert_candidate_for_test(CandidateRecord {
            candidate_id: CandidateId::from_persisted("cand-c1-1".to_owned()),
            workspace_id: child.workspace.id.clone(),
            producing_agent_id: child.agent_id.clone(),
            producing_attempt_id: child.attempt_id.clone(),
            base_commit: "base".into(),
            base_content: "tree-base".into(),
            base_change_id: Some("qqqq".into()),
            commit: "sealed".into(),
            content: "tree-sealed".into(),
            change_id: Some("zzzz".into()),
            sealed_at: 7,
            adopted_by: None,
            merged_at: Some("target-after-snapshot".to_owned()),
            adopted_evidence: vec!["c9-a1".to_owned()],
        });
        tree.set_workspace_base_for_test(&child.workspace.id, "base", "tree-base", Some("qqqq"));
        save_tree(&paths, &mut tree).unwrap();

        let loaded = load_tree(&paths, "tree-rows").unwrap().expect("tree");
        assert_eq!(loaded.attempts(), tree.attempts());
        assert_eq!(loaded.candidates(), tree.candidates());
        let workspace = loaded.workspace(&child.workspace.id).unwrap();
        assert_eq!(workspace.base_commit.as_deref(), Some("base"));
        assert_eq!(workspace.base_content.as_deref(), Some("tree-base"));
        assert_eq!(
            workspace.base_change_id.as_deref(),
            Some("qqqq"),
            "the jj base change survives the round trip; adoption starts its range there"
        );
        assert_eq!(workspace.status, WorkspaceStatus::Active);
        assert_eq!(workspace.adopter_agent_id, None);
        assert_eq!(loaded.node(&child.agent_id).unwrap().tier, Tier::ReadOnly);
        assert_eq!(loaded.node("root").unwrap().attempt_id, None);
        assert_eq!(loaded.node("root").unwrap().template, None);

        // The adopter of an in-flight landing is durable, so a reopen faults
        // the caller whose checkout it writes.
        tree.set_workspace_adopting_for_test(&child.workspace.id, "root");
        save_tree(&paths, &mut tree).unwrap();
        let loaded = load_tree(&paths, "tree-rows").unwrap().expect("tree");
        let workspace = loaded.workspace(&child.workspace.id).unwrap();
        assert_eq!(workspace.status, WorkspaceStatus::Adopting);
        assert_eq!(workspace.adopter_agent_id.as_deref(), Some("root"));
    }

    #[test]
    fn orchestration_snapshot_roundtrip_preserves_max_depth_and_digest() {
        use crate::product_config::ProductConfig;
        use plakous_hosted::agent_tree::ProductAgentTree;
        use plakous_hosted::agent_tree_protocol::{MAX_TREE_DEPTH, WriterMode};

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let config = ProductConfig::parse("[orchestration]\nmax_depth = 4\n").unwrap();
        let snapshot = crate::roster_input::snapshot_from_config(
            &config,
            "grok",
            "grok-4.6",
            Some("low".into()),
        );
        assert_eq!(snapshot.max_depth, MAX_TREE_DEPTH);
        assert!(snapshot.digest_matches());

        let mut tree = ProductAgentTree::bootstrap_root(
            "tree-snap",
            "root",
            "owner",
            root.path().to_path_buf(),
            "grok-4.6",
            WriterMode::SingleLease,
        )
        .unwrap();
        tree.install_snapshot(snapshot);
        save_tree(&paths, &mut tree).unwrap();

        let loaded = load_tree(&paths, "tree-snap").unwrap().expect("tree");
        assert_eq!(loaded.snapshot.max_depth, MAX_TREE_DEPTH);
        assert!(loaded.snapshot.digest_matches());
        assert_eq!(
            loaded.grants().get("root").unwrap().max_depth,
            MAX_TREE_DEPTH
        );
    }

    /// Schema 5 carries the nested roster: the working set, every closure
    /// card and each card's `spawns` list come back exactly, under the same
    /// digest.
    #[test]
    fn a_nested_spawn_roster_round_trips_at_tree_schema_7() {
        use crate::product_config::{AgentTemplate, ProductConfig};
        use plakous_hosted::agent_tree::ProductAgentTree;
        use plakous_hosted::agent_tree_protocol::{TemplateName, WorkingSet, WriterMode};

        assert_eq!(plakous_recursive_subagents_store::SCHEMA_VERSION, 8);
        let name = |name: &str| TemplateName::parse(name).unwrap();
        let card = |spawns: Vec<TemplateName>| AgentTemplate {
            account: "codex".to_owned(),
            model: "gpt-5.6-luna".to_owned(),
            description: "a card".to_owned(),
            may_adopt: !spawns.is_empty(),
            spawns,
            ..AgentTemplate::default()
        };
        let mut config = ProductConfig::default_product();
        config
            .templates
            .insert("lead".to_owned(), card(vec![name("helper")]));
        config
            .templates
            .insert("helper".to_owned(), card(Vec::new()));
        config.working_set = WorkingSet::parse(["lead"]).unwrap();
        let snapshot =
            crate::roster_input::snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut tree = ProductAgentTree::bootstrap_root(
            "tree-nested",
            "root",
            "owner",
            root.path().to_path_buf(),
            "gpt-5.6-luna",
            WriterMode::SingleLease,
        )
        .unwrap();
        tree.install_snapshot(snapshot.clone());
        save_tree(&paths, &mut tree).unwrap();

        let loaded = load_tree(&paths, "tree-nested").unwrap().expect("tree");
        assert_eq!(loaded.snapshot, snapshot);
        assert!(loaded.snapshot.digest_matches());
        assert_eq!(
            loaded.snapshot.binding(&name("lead")).unwrap().spawns,
            [name("helper")]
        );
        assert_eq!(
            loaded
                .snapshot
                .spawnable_template_names(None)
                .collect::<Vec<_>>(),
            [&name("lead")]
        );
    }

    /// Schema 5 carries the command intents with their feed-derived facts,
    /// and the tree projector's cursors, exactly.
    #[test]
    fn command_intents_and_feed_cursors_round_trip_at_tree_schema_6() {
        use plakous_hosted::agent_tree::{AgentCommand, AgentCommandKind, ProductAgentTree};
        use plakous_hosted::agent_tree_protocol::WriterMode;

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut tree = ProductAgentTree::bootstrap_root(
            "tree-commands",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        for (id, kind) in [
            ("plak:wake:1", AgentCommandKind::RootBatch),
            ("user:2", AgentCommandKind::RootBatch),
        ] {
            tree.record_command(AgentCommand::intent(
                id,
                "root",
                None,
                kind,
                format!("text of {id}"),
                Vec::new(),
            ))
            .unwrap();
        }
        assert_eq!(
            tree.join_pending_root_commands(&["user:2".to_owned()])
                .len(),
            1
        );
        tree.seed_feed_cursor("session-a", 7);
        save_tree(&paths, &mut tree).unwrap();

        let loaded = load_tree(&paths, "tree-commands").unwrap().expect("tree");
        assert_eq!(loaded.commands(), tree.commands());
        assert!(loaded.command("plak:wake:1").unwrap().withdrawn);
        assert_eq!(loaded.feed_cursor("session-a"), 7);
        assert_eq!(loaded.feed_cursors(), tree.feed_cursors());
    }

    #[test]
    fn agent_tree_account_and_effort_round_trip() {
        use plakous_hosted::agent_tree::ProductAgentTree;
        use plakous_hosted::agent_tree_protocol::SpawnAgentRequest;
        use plakous_hosted::agent_tree_protocol::WriterMode;

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut tree = ProductAgentTree::bootstrap_root(
            "tree-bind",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        tree.pin_bind_hints_for_test(crate::roster_input::product_bind_hints());
        tree.set_node_account_for_test("root", "codex");
        pin_review(&mut tree, "grok", "grok-4", Some("high"));
        let spawn_request = SpawnAgentRequest::test_review();
        let child = tree.spawn_agent("root", &spawn_request, None).unwrap();
        save_tree(&paths, &mut tree).unwrap();

        let loaded = load_tree(&paths, "tree-bind").unwrap().expect("tree");
        let node = loaded.node(&child.agent_id).unwrap();
        assert_eq!(node.account, "grok");
        assert_eq!(node.tuple_id, "grok-4");
        assert_eq!(node.effort.as_deref(), Some("high"));
    }

    #[test]
    fn agent_tree_duplicate_child_ids_across_trees_persist() {
        use plakous_hosted::agent_tree::ProductAgentTree;
        use plakous_hosted::agent_tree_protocol::{
            SendAgentMessageRequest, SpawnAgentRequest, WriterMode,
        };

        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let spawn_request = SpawnAgentRequest::test_review();
        let mut grok_tree = ProductAgentTree::bootstrap_root(
            "tree-grok",
            "root-grok",
            "owner",
            root.path().join("grok"),
            "tuple-grok",
            WriterMode::SingleLease,
        )
        .unwrap();
        pin_review(&mut grok_tree, "grok", "tuple-grok", None);
        let grok_child = grok_tree
            .spawn_agent("root-grok", &spawn_request, None)
            .unwrap();
        let mut claude_tree = ProductAgentTree::bootstrap_root(
            "tree-claude",
            "root-claude",
            "owner",
            root.path().join("claude"),
            "tuple-claude",
            WriterMode::SingleLease,
        )
        .unwrap();
        pin_review(&mut claude_tree, "claude", "tuple-claude", None);
        let claude_child = claude_tree
            .spawn_agent("root-claude", &spawn_request, None)
            .unwrap();
        assert_eq!(grok_child.agent_id, "c1");
        assert_eq!(claude_child.agent_id, "c1");
        let mail = SendAgentMessageRequest {
            agent_id: "c1".into(),
            message: "coordinate once".into(),
            purpose: None,
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        };
        let grok_mail = grok_tree
            .send_agent_message("root-grok", &mail, None)
            .unwrap();
        let claude_mail = claude_tree
            .send_agent_message("root-claude", &mail, None)
            .unwrap();
        assert_eq!(grok_mail.message_id, "m1");
        assert_eq!(claude_mail.message_id, "m1");
        save_tree(&paths, &mut grok_tree).unwrap();
        save_tree(&paths, &mut claude_tree).unwrap();

        let loaded_grok = load_tree(&paths, "tree-grok")
            .unwrap()
            .expect("grok tree persisted");
        let loaded_claude = load_tree(&paths, "tree-claude")
            .unwrap()
            .expect("claude tree persisted");
        assert_eq!(loaded_grok.node("c1").unwrap().description, "reviewer");
        assert_eq!(loaded_claude.node("c1").unwrap().description, "reviewer");
        assert_ne!(
            loaded_grok.node("c1").unwrap().workspace_id,
            loaded_claude.node("c1").unwrap().workspace_id
        );
        assert_eq!(
            loaded_grok.messages().get("m1").unwrap().body,
            "coordinate once"
        );
        assert_eq!(
            loaded_claude.messages().get("m1").unwrap().body,
            "coordinate once"
        );
    }

    /// The tree store is what a hub miss reloads from, so the reload has to
    /// produce a tree the topology tools still compile over.
    mod hub_reload {
        use std::sync::Arc;

        use plakous_recursive_subagents::hub::test_support::unique_tree_id;
        use plakous_recursive_subagents::hub::{force_release, insert_root_with, lookup};
        use plakous_recursive_subagents::protocol::WriterMode;
        use plakous_recursive_subagents::tree::ProductAgentTree;

        use super::super::{load_tree, save_tree};
        use crate::home::paths::PlakPaths;

        #[tokio::test]
        async fn hub_miss_reloads_store_and_compiles_root_topology_tools() {
            let (_tmp, paths) = PlakPaths::isolated_test_home();
            paths.ensure_layout().unwrap();
            let tree_id = unique_tree_id();
            let mut tree = ProductAgentTree::bootstrap_root_for_test(
                &tree_id,
                &tree_id,
                "root",
                paths.home().to_path_buf(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            save_tree(&paths, &mut tree).unwrap();
            assert!(lookup(&tree_id).is_none());
            let loaded = load_tree(&paths, &tree_id)
                .unwrap()
                .expect("persisted tree");
            assert!(
                loaded
                    .compiled_tool_names(&tree_id)
                    .unwrap()
                    .contains(&"spawn_agent")
            );
            assert!(loaded.compiled_tool_names("successor-session").is_err());
            let runtime = insert_root_with(
                tree_id.clone(),
                loaded,
                super::super::tree_store(&paths).unwrap(),
                None,
            )
            .unwrap();
            let extensions = plakous_hosted::topology_extensions::topology_tool_extensions(
                Arc::clone(runtime.tree_handle_for_test()),
                tree_id.as_str(),
            )
            .unwrap();
            let names = extensions
                .names()
                .map(plakous::model::ToolName::as_str)
                .collect::<Vec<_>>();
            assert!(names.contains(&"spawn_agent"));
            assert!(names.contains(&"send_agent_message"));
            assert!(names.contains(&"wait_agent"));
            force_release(&tree_id).await;
        }
    }
}
