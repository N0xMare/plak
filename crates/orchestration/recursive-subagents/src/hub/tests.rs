#![allow(
    clippy::too_many_lines,
    reason = "hub scenarios retain release barriers, deadlines and exact-slot assertions through each full retirement"
)]

use super::test_support;
use super::test_support::{bootstrap, memory_store, unique_tree_id};
use super::*;

use crate::tree::ProductAgentTree;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct UsageObserver(Mutex<Vec<crate::tree::ProjectedChildTurnUsage>>);

impl TopologyObserver for UsageObserver {
    fn turn_usage(&self, terminal: crate::tree::ProjectedChildTurnUsage) {
        self.0.lock().unwrap().push(terminal);
    }
    fn node_spawned(&self, _: &str, _: &str, _: Option<&str>, _: &str) {}
    fn node_detached(&self, _: &str, _: &str, _: Option<&str>, _: crate::tree::NodeDetach) {}
    fn epoch_advanced(&self, _: &str) {}
}

#[test]
fn release_locator_is_derived_from_its_feed_not_its_returned_locator() {
    use crate::protocol::SpawnAgentRequest;
    use crate::tree::{TreeFeed, TreeFeedReads};
    use plakous::sessions::feed::{FeedFact, FeedHeader, FeedRecord, FeedTurnOutcome};

    let root = tempfile::tempdir().unwrap();
    let observer = Arc::new(UsageObserver::default());
    let runtime = ProductTopologyRuntime::new(
        bootstrap("release-feed", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        Some(observer.clone()),
    );
    let session = "00000000-0000-4000-8000-0000000000c1";
    let branch = "00000000-0000-4000-8000-00000000000b";
    let head = "00000000-0000-4000-8000-000000003001";
    let locator = format!("{session}/{branch}/{head}");
    let child = runtime
        .mutate_then_emit(false, |tree| {
            let child = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map_err(|error| error.message)?
                .agent_id;
            tree.record_child_session(&child, &locator)
                .map_err(|error| error.message)?;
            Ok(child)
        })
        .unwrap();
    publish_release(
        runtime.driver(),
        &child,
        LiveRelease {
            locator: Some("not-feed-authority".to_owned()),
            ..LiveRelease::default()
        },
    )
    .unwrap();
    runtime
        .read(|tree| assert!(tree.node(&child).unwrap().session_locator.is_none()))
        .unwrap();

    let header: FeedHeader = serde_json::from_value(serde_json::json!({
        "session_id": session,
        "seq": 1,
        "branch_id": branch,
        "committed_at_ms": 1000,
        "change": {"lease_released": {"head": head, "fence": 1}},
    }))
    .unwrap();
    let feed = TreeFeed {
        session_id: session.to_owned(),
        retained_from: 1,
        below_retained: None,
        heads: Vec::new(),
        head_turns: std::collections::BTreeMap::default(),
        records: vec![FeedRecord::from_store(
            header,
            vec![FeedFact::TurnTerminal {
                turn_id: "00000000-0000-4000-8000-000000001001".parse().unwrap(),
                command_id: CommandId::new("release-usage").unwrap(),
                outcome: FeedTurnOutcome::Completed {
                    finish: plakous::model::FinishReason::Stop,
                },
                closed_exchange: true,
                steering_settled_unapplied: false,
                usage: plakous::agent::TurnUsage::new(
                    plakous::model::Usage::new(123, 45, 0, 0, 0),
                    1,
                    1,
                    0,
                )
                .unwrap(),
            }],
        )],
        reads: TreeFeedReads::default(),
    };
    publish_release(
        runtime.driver(),
        &child,
        LiveRelease {
            locator: Some("still-not-feed-authority".to_owned()),
            feed: Some(feed.clone()),
            failure: None,
        },
    )
    .unwrap();
    let emitted = observer.0.lock().unwrap().clone();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].usage.model().input_tokens(), 123);
    assert_eq!(emitted[0].committed_at_ms, 1000);
    publish_release(
        runtime.driver(),
        &child,
        LiveRelease {
            feed: Some(feed.clone()),
            ..LiveRelease::default()
        },
    )
    .unwrap();
    assert_eq!(*observer.0.lock().unwrap(), emitted);
    runtime
        .read(|tree| {
            assert_eq!(
                tree.node(&child).unwrap().session_locator.as_deref(),
                Some(locator.as_str())
            );
            assert_eq!(tree.feed_cursor(session), 1);
            let mut rebuilt = tree.clone();
            rebuilt.clear_feed_views();
            assert_eq!(feed.apply(&mut rebuilt, &child).unwrap().applied, 1);
            assert_eq!(
                rebuilt.node(&child).unwrap().session_locator,
                tree.node(&child).unwrap().session_locator
            );
            assert_eq!(feed.apply(&mut rebuilt, &child).unwrap().applied, 0);
        })
        .unwrap();
}

#[tokio::test]
async fn lookup_returns_the_same_tree_arc() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    let first = insert_root(
        tree_id.clone(),
        bootstrap(&tree_id, root.path().to_path_buf()),
        None,
    )
    .unwrap();
    let second = lookup(&tree_id).expect("hub should occupy after insert");
    assert!(Arc::ptr_eq(
        first.tree_handle_for_test(),
        second.tree_handle_for_test()
    ));
    assert!(Arc::ptr_eq(&first.live, &second.live));
    force_release(&tree_id).await;
    assert!(lookup(&tree_id).is_none());
}

/// A hub hit — root or child, attaching or not
/// — asks for no store and loads nothing.
#[tokio::test]
async fn hub_hit_asks_for_no_store() {
    let root = tempfile::tempdir().unwrap();
    test_support::hub_hit_opens_no_store(root.path().to_path_buf()).await;
}

#[tokio::test]
async fn child_resolve_does_not_load_or_pause() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    let runtime = insert_root(
        tree_id.clone(),
        bootstrap(&tree_id, root.path().to_path_buf()),
        None,
    )
    .unwrap();
    let generation = runtime.driver().lock().unwrap().host_generation;
    let state = runtime.driver().lock().unwrap().state;
    let loaded = AtomicBool::new(false);
    let resolved = resolve_topology_runtime(
        &tree_id,
        false,
        TopologyHubAttach::LookupOnly,
        || Ok(memory_store()),
        None,
        |_| {
            loaded.store(true, Ordering::SeqCst);
            Err(TreeRuntimeError::new("child compile must not load_tree"))
        },
    )
    .unwrap();
    assert!(!loaded.load(Ordering::SeqCst));
    assert!(Arc::ptr_eq(
        runtime.tree_handle_for_test(),
        resolved.tree_handle_for_test()
    ));
    {
        let tree = resolved.tree_handle_for_test().lock().unwrap();
        assert_eq!(tree.host_generation, generation);
        assert_eq!(tree.state, state);
    }
    force_release(&tree_id).await;
}

#[tokio::test]
async fn missing_hub_child_fails_closed_without_bootstrap() {
    let tree_id = unique_tree_id();
    let loaded = AtomicBool::new(false);
    let error = resolve_topology_runtime(
        &tree_id,
        false,
        TopologyHubAttach::LookupOnly,
        || Ok(memory_store()),
        None,
        |_| {
            loaded.store(true, Ordering::SeqCst);
            Err(TreeRuntimeError::new("must not bootstrap"))
        },
    )
    .err()
    .expect("child compile without hub must fail closed");
    assert!(!loaded.load(Ordering::SeqCst));
    assert!(
        error
            .message()
            .contains("child catalog compile cannot load")
    );
    assert!(lookup(&tree_id).is_none());
}

#[tokio::test]
async fn missing_hub_root_lookup_does_not_bootstrap() {
    let tree_id = unique_tree_id();
    let loaded = AtomicBool::new(false);
    let error = resolve_topology_runtime(
        &tree_id,
        true,
        TopologyHubAttach::LookupOnly,
        || Ok(memory_store()),
        None,
        |_| {
            loaded.store(true, Ordering::SeqCst);
            Err(TreeRuntimeError::new("must not bootstrap"))
        },
    )
    .err()
    .expect("root lookup-only without hub must fail closed");
    assert!(!loaded.load(Ordering::SeqCst));
    assert!(error.message().contains("outside session attach"));
    assert!(lookup(&tree_id).is_none());
}

#[tokio::test]
async fn missing_hub_child_does_not_clobber_durable_tree() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    let store = memory_store();
    let mut tree = bootstrap(&tree_id, root.path().to_path_buf());
    tree.spawn_agent(
        "root",
        &crate::protocol::SpawnAgentRequest::test_review(),
        None,
    )
    .unwrap();
    tree.acknowledge_revision(store.save(&tree).unwrap());

    let error = resolve_topology_runtime(
        &tree_id,
        false,
        TopologyHubAttach::AttachRoot,
        || Ok(memory_store()),
        None,
        |_| Ok(bootstrap(&tree_id, root.path().to_path_buf())),
    )
    .err()
    .expect("child miss must not bootstrap even when attach is requested");
    assert!(
        error
            .message()
            .contains("child catalog compile cannot load")
    );

    let loaded = ProductAgentTree::reopen(store.load(&tree_id).unwrap().unwrap());
    assert!(loaded.node("c1").is_ok());
}

#[tokio::test]
async fn attach_root_load_runs_once_then_lookup_shares() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    let loads = AtomicUsize::new(0);
    let first = resolve_topology_runtime(
        &tree_id,
        true,
        TopologyHubAttach::AttachRoot,
        || Ok(memory_store()),
        None,
        |_| {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(bootstrap(&tree_id, root.path().to_path_buf()))
        },
    )
    .unwrap();
    let second = resolve_topology_runtime(
        &tree_id,
        true,
        TopologyHubAttach::AttachRoot,
        || Ok(memory_store()),
        None,
        |_| {
            loads.fetch_add(1, Ordering::SeqCst);
            Err(TreeRuntimeError::new("second attach must not reload"))
        },
    )
    .unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(
        first.tree_handle_for_test(),
        second.tree_handle_for_test()
    ));
    force_release(&tree_id).await;
}

#[tokio::test]
async fn attach_root_resolve_acquires_one_lease() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    resolve_topology_runtime(
        &tree_id,
        true,
        TopologyHubAttach::AttachRoot,
        || Ok(memory_store()),
        None,
        |_| Ok(bootstrap(&tree_id, root.path().to_path_buf())),
    )
    .unwrap();
    release_root_lease(&tree_id).await.unwrap();
    assert!(lookup(&tree_id).is_none());
}

#[tokio::test]
async fn overlapping_root_leases_keep_the_hub() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    insert_root(
        tree_id.clone(),
        bootstrap(&tree_id, root.path().to_path_buf()),
        None,
    )
    .unwrap();
    process_hub().acquire_root_lease(&tree_id).unwrap();
    process_hub().acquire_root_lease(&tree_id).unwrap();
    release_root_lease(&tree_id).await.unwrap();
    assert!(lookup(&tree_id).is_some());
    release_root_lease(&tree_id).await.unwrap();
    assert!(lookup(&tree_id).is_none());
}

#[tokio::test]
async fn child_release_does_not_drop_hub() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    insert_root(
        tree_id.clone(),
        bootstrap(&tree_id, root.path().to_path_buf()),
        None,
    )
    .unwrap();
    process_hub().acquire_root_lease(&tree_id).unwrap();
    release_unleased(&tree_id).await;
    assert!(lookup(&tree_id).is_some());
    release_root_lease(&tree_id).await.unwrap();
    assert!(lookup(&tree_id).is_none());
}

#[tokio::test]
async fn failed_attach_release_unleased_removes_hub() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    insert_root(
        tree_id.clone(),
        bootstrap(&tree_id, root.path().to_path_buf()),
        None,
    )
    .unwrap();
    release_unleased(&tree_id).await;
    assert!(lookup(&tree_id).is_none());
}

/// Every child cancel is bounded by the same settle window as the root's.
///
/// `TurnControl::cancel` waits until the runtime settles, so a bare `await`
/// on it lets a child whose provider ignores the token hang stop-all and
/// the host loop with it. A `TurnControl` cannot be
/// constructed offline, so the law is pinned at the source: this module has
/// no unbounded `control.cancel()` await left.
#[test]
fn child_cancels_are_bounded_by_the_host_settle_timeout() {
    let source = concat!(
        include_str!("../hub.rs"),
        include_str!("registry.rs"),
        include_str!("runtime.rs"),
        include_str!("shutdown.rs")
    );
    assert!(
        source.contains("plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT"),
        "the child cancel must go through the shared settle window"
    );
    assert!(
        !source.contains(".cancel()\n            .await"),
        "an unbounded child cancel await is back"
    );
}

#[tokio::test]
async fn cancel_live_missing_agent_is_idempotent() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    let runtime = insert_root(
        tree_id.clone(),
        bootstrap(&tree_id, root.path().to_path_buf()),
        None,
    )
    .unwrap();
    // Nothing was ever live here, so every cancel is `NotLive` — not a
    // failure, and not a cancel either. Folding both into `Ok(())` is
    // what hid the difference from the close path.
    assert_eq!(
        runtime.cancel_live("c1").await.unwrap(),
        LiveCancelOutcome::NotLive
    );
    let report = runtime.cancel_live_many(&["c1".into(), "c2".into()]).await;
    assert!(report.is_clean(), "a missing agent is not a failure");
    assert!(report.cancelled.is_empty(), "nothing was running to stop");
    assert_eq!(report.not_live, vec!["c1".to_owned(), "c2".to_owned()]);
    force_release(&tree_id).await;
}

#[tokio::test]
async fn crash_cut_reattach_is_single_flight() {
    let tree_id = unique_tree_id();
    let root = tempfile::tempdir().unwrap();
    let runtime = insert_root(
        tree_id.clone(),
        bootstrap(&tree_id, root.path().to_path_buf()),
        None,
    )
    .unwrap();
    let held = runtime.lock_reattach().await;
    assert!(runtime.try_lock_reattach().is_err());
    let shared = lookup(&tree_id).expect("hub remains");
    assert!(shared.try_lock_reattach().is_err());
    drop(held);
    assert!(runtime.try_lock_reattach().is_ok());
    force_release(&tree_id).await;
}

/// Quit releases every child of the tree in one bounded wait, not
/// one after another. Timing-free: the release futures are handed to a
/// single `join_all` under one timeout, so the teardown's worst case is
/// the slowest child rather than their sum.
/// The live table keeps the same invariant when handed
/// to the product: the closed flag and the live map are one lock, so a
/// turn racing a shutdown is either in the set the drain takes or refused
/// by the flag the drain already set — never left running with nothing
/// holding it.
///
/// Driven against the real map: the test holds the live lock, which is
/// exactly the window a racing shutdown would try to use, and asserts the
/// decision flips under it.
#[tokio::test]
async fn a_turn_racing_shutdown_is_drained_or_refused_but_never_orphaned() {
    let tree_id = unique_tree_id();
    let runtime = insert_root(
        tree_id.clone(),
        bootstrap(&tree_id, std::env::temp_dir()),
        None,
    )
    .unwrap();

    {
        let live = runtime.live.lock().await;
        assert!(
            runtime.admits_live(&live, "c1"),
            "an idle agent on a live tree is admitted"
        );
    }

    // The shutdown sets the flag before it takes the lock to drain. Hold
    // the lock across the flag so the racing window is real, then assert
    // the decision a racing admission would take inside it.
    let held = runtime.live.lock().await;
    runtime.close_for_shutdown();
    assert!(
        !runtime.admits_live(&held, "c1"),
        "an admission that wins the lock after the flag is set must refuse"
    );
    drop(held);

    assert!(
        runtime.live.lock().await.is_empty(),
        "no child was admitted before shutdown"
    );
    assert!(
        runtime.is_closed(),
        "the flag stays set: nothing is admitted after a drain"
    );
    force_release(&tree_id).await;
}

/// The closed flag is not consulted beside the map: `try_occupy_live`
/// re-reads it *inside* the lock it inserts under, so a check that
/// happened before the lock cannot be the one the insert relies on.
#[test]
fn the_closed_flag_is_read_under_the_lock_the_insert_takes() {
    let source = include_str!("runtime.rs");
    let admits = source
        .split("fn admits_live(")
        .nth(1)
        .expect("admits_live is the one admission decision");
    let body = &admits[..admits.find("\n    }\n").unwrap_or(admits.len())];
    assert!(
        body.contains("self.is_closed()") && body.contains("live.contains_key"),
        "the admission reads the flag and the map together"
    );
    let occupy = source
        .split("pub async fn try_occupy_live_admitted(")
        .nth(1)
        .expect("try_occupy_live_admitted is the admission entry");
    let occupy = &occupy[..occupy.find("\n    }\n").unwrap_or(occupy.len())];
    let lock_at = occupy
        .find("self.live.lock()")
        .expect("the admission takes the live lock");
    let decide_at = occupy
        .find("self.admits_live(")
        .expect("the admission goes through the one decision");
    assert!(
        lock_at < decide_at,
        "the decision must be taken with the live lock held"
    );
    let insert_at = occupy
        .find("live.children.insert(")
        .expect("the admission inserts");
    assert!(
        decide_at < insert_at,
        "the decision must precede the insert it guards"
    );
}

struct RefusingStore;
struct AttachProbeStore {
    hub: std::sync::Weak<TopologyHub>,
    inner: crate::tree::MemoryTreeStore,
    saves: AtomicUsize,
}

impl TreeStore for AttachProbeStore {
    fn exists(&self, id: &str) -> Result<bool, crate::tree::TreeStoreError> {
        self.inner.exists(id)
    }
    fn ids(&self) -> Result<Vec<String>, crate::tree::TreeStoreError> {
        self.inner.ids()
    }
    fn load(
        &self,
        id: &str,
    ) -> Result<Option<crate::tree::PersistedTree>, crate::tree::TreeStoreError> {
        self.inner.load(id)
    }
    fn save(
        &self,
        tree: &ProductAgentTree,
    ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
        assert!(
            self.hub.upgrade().unwrap().entries.try_lock().is_ok(),
            "attach saved under the hub entries lock"
        );
        self.saves.fetch_add(1, Ordering::SeqCst);
        self.inner.save(tree)
    }
}

#[test]
fn attach_startup_is_unlocked_and_skips_an_empty_consequence() {
    let root = tempfile::tempdir().unwrap();
    let hub = Arc::new(TopologyHub::new());
    let store = Arc::new(AttachProbeStore {
        hub: Arc::downgrade(&hub),
        inner: crate::tree::MemoryTreeStore::new(),
        saves: AtomicUsize::new(0),
    });
    let vcs = Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new());
    let tree = bootstrap("attach-probe", root.path().to_path_buf());
    let runtime = hub
        .insert_root(
            "attach-probe",
            tree.clone(),
            store.clone(),
            vcs.clone(),
            None,
        )
        .unwrap();
    assert_eq!(store.saves.load(Ordering::SeqCst), 1);
    let occupant = hub
        .insert_root("attach-probe", tree, store.clone(), vcs, None)
        .unwrap();
    assert!(Arc::ptr_eq(&runtime, &occupant));
    assert_eq!(store.saves.load(Ordering::SeqCst), 1);
}

impl TreeStore for RefusingStore {
    fn exists(&self, _: &str) -> Result<bool, crate::tree::TreeStoreError> {
        Ok(false)
    }
    fn ids(&self) -> Result<Vec<String>, crate::tree::TreeStoreError> {
        Ok(vec![])
    }
    fn save(
        &self,
        _: &ProductAgentTree,
    ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
        Err(crate::tree::TreeStoreError::unavailable(
            "initial publish refused",
        ))
    }
    fn load(
        &self,
        _: &str,
    ) -> Result<Option<crate::tree::PersistedTree>, crate::tree::TreeStoreError> {
        Ok(None)
    }
}

#[test]
fn initial_publish_refusal_does_not_mutate_or_dispatch_queued_seal() {
    use crate::protocol::{AttemptScopedRequest, NodeStatus, SpawnAgentRequest, Tier, WriterMode};
    use plakous_workspace::occupancy_vcs::fake::FakeVcs;
    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        "refused-seal",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut binding = crate::epoch::RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    tree.pin_template("implement", binding);
    let child = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("implement", Tier::Mutate, "impl", "edit").with_fork(),
            None,
        )
        .unwrap()
        .agent_id;
    let runtime = ProductTopologyRuntime::new(tree, Arc::new(RefusingStore), vcs.clone(), None);
    let error = runtime
        .mutate_then_emit(false, |tree| {
            tree.close_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .map_err(|error| error.message)
        })
        .unwrap_err();
    assert!(error.contains("initial publish refused"));
    let mut tree = runtime.driver().lock().unwrap();
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Running);
    assert!(tree.attempt(&child).unwrap().candidate_id.is_none());
    assert!(
        vcs.calls_of(plakous_workspace::occupancy_vcs::fake::FakeOp::Seal)
            .is_empty()
    );
    assert!(
        tree.take_effects().is_empty(),
        "no cleanup may remain stranded"
    );
}

#[test]
fn initial_publish_refusal_dispatches_no_hook_or_removal() {
    use crate::protocol::{NodeStatus, SpawnAgentRequest, Tier, WriterMode};
    use plakous_workspace::occupancy_vcs::{
        VcsError,
        fake::{FakeOp, FakeVcs},
    };
    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        "refused-hook",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut binding = crate::epoch::RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    binding.on_mint = Some("fixture-hook".into());
    tree.pin_template("implement", binding);
    let child = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("implement", Tier::Mutate, "impl", "edit").with_fork(),
            None,
        )
        .unwrap()
        .agent_id;
    let path = tree.node(&child).unwrap().path.clone();
    vcs.hook_next_with(Err(VcsError::hook_failed("fixture failure")));
    let runtime = ProductTopologyRuntime::new(tree, Arc::new(RefusingStore), vcs.clone(), None);
    assert!(
        runtime
            .mutate_then_emit(false, |_| Ok(()))
            .unwrap_err()
            .contains("initial publish refused")
    );
    assert!(vcs.calls_of(FakeOp::Hook).is_empty());
    assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
    assert!(path.exists());
    let mut tree = runtime.driver().lock().unwrap();
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Running);
    assert!(
        !tree.take_effects().is_empty(),
        "unpublished hook remains queued"
    );
}

#[test]
fn effect_free_mutation_publishes_acknowledged_revision() {
    let root = tempfile::tempdir().unwrap();
    let store = memory_store();
    let runtime = ProductTopologyRuntime::new(
        bootstrap("effect-free", root.path().to_path_buf()),
        store.clone(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    );
    runtime
        .mutate_then_emit(false, |tree| {
            tree.record_fault("root", "durable fault");
            Ok(())
        })
        .unwrap();
    assert!(store.load("effect-free").unwrap().is_some());
    runtime
        .read(|tree| {
            assert_eq!(tree.revision, tree.stored_revision);
            assert_eq!(tree.fault_reason("root"), Some("durable fault"));
        })
        .unwrap();
}

struct BarrierChild {
    panic_on_release: bool,
    failure: Option<String>,
    turn: TurnId,
    entered: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct DetachObserver(Mutex<Vec<crate::tree::NodeDetach>>);

impl TopologyObserver for DetachObserver {
    fn turn_usage(&self, _: crate::tree::ProjectedChildTurnUsage) {}
    fn node_spawned(&self, _: &str, _: &str, _: Option<&str>, _: &str) {}
    fn node_detached(&self, _: &str, _: &str, _: Option<&str>, status: crate::tree::NodeDetach) {
        self.0.lock().unwrap().push(status);
    }
    fn epoch_advanced(&self, _: &str) {}
}

#[tokio::test(start_paused = true)]
async fn retirement_owner_emits_one_recovery_detach_and_notice_despite_late_failure_and_joiners() {
    let root = tempfile::tempdir().unwrap();
    let observer = Arc::new(DetachObserver::default());
    let runtime = ProductTopologyRuntime::new(
        bootstrap("retirement-observer", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        Some(observer.clone()),
    );
    let id = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &crate::protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        runtime
            .try_occupy_live(
                id.clone(),
                Box::new(BarrierChild {
                    panic_on_release: false,
                    failure: Some("release refused".into()),
                    turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    entered: entered.clone(),
                    finish: finish.clone()
                })
            )
            .await
            .is_ok()
    );
    let mut faults = runtime.observe_retirement_faults();
    let first = runtime
        .begin_cancel_live_many(std::slice::from_ref(&id))
        .await;
    let second = runtime
        .begin_cancel_live_many(std::slice::from_ref(&id))
        .await;
    entered.notified().await;
    tokio::time::advance(HOST_RETIRE_JOIN_TIMEOUT + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    let (agent, message) = tokio::time::timeout(Duration::from_secs(1), faults.recv())
        .await
        .expect("owner fault notice")
        .unwrap();
    assert_eq!(agent, id);
    assert!(message.contains("recover_agent required"));
    finish.notify_one();
    assert!(!first.await.is_clean());
    assert!(!second.await.is_clean());
    // Join the actual completion, not the expired bounded joiners.
    while runtime.live.lock().await.contains_key(&id) {
        tokio::task::yield_now().await;
    }
    assert!(faults.try_recv().is_err());
    assert_eq!(
        *observer.0.lock().unwrap(),
        [crate::tree::NodeDetach::RecoveryRequired]
    );
}
impl LiveChildHandle for BarrierChild {
    fn agent(&self) -> Option<Agent> {
        None
    }
    fn turn_id(&self) -> TurnId {
        self.turn
    }
    fn turn_control(&self) -> TurnControl {
        panic!("control is not used by this fixture")
    }
    fn follow_up(
        &self,
        _: CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Err("fixture".into()) })
    }

    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        None
    }
    fn restart(
        &mut self,
        _: TreeDriver,
        _: CommandId,
        _: String,
        _delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
        Box::pin(async { Err("fixture".into()) })
    }
    fn release(
        self: Box<Self>,
        driver: TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        self.cancel_and_release(driver, None)
    }
    fn cancel_and_release(
        self: Box<Self>,
        _driver: TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        if let Some(dispatched) = dispatched {
            let _ = dispatched.send(());
        }
        Box::pin(async move {
            self.entered.notify_one();
            self.finish.notified().await;
            assert!(!self.panic_on_release, "injected child release panic");
            LiveRelease {
                feed: None,
                locator: Some("released-locator".into()),
                failure: self.failure,
            }
        })
    }
}

#[tokio::test(start_paused = true)]
async fn joiner_deadline_does_not_refault_a_settled_release() {
    use crate::protocol::{AttemptStatus, SpawnAgentRequest};
    use crate::tree::RecoveryResolution;
    use std::task::{Context, Poll};

    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("joiner-race", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let id = runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map(|child| child.agent_id)
                .map_err(|error| error.message)
        })
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        runtime
            .try_occupy_live(
                id.clone(),
                Box::new(BarrierChild {
                    panic_on_release: false,
                    failure: None,
                    turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let child = id.clone();
    let joiner = Box::pin(async move { owner.cancel_live(&child).await });
    let joiner = Arc::new(Mutex::new(Some(joiner)));
    let worker_joiner = joiner.clone();
    let weak = Arc::downgrade(&runtime);
    let child = id.clone();
    runtime
        .set_retirement_recovery(Arc::new(move || {
            let pending = joiner.lock().unwrap().take();
            let Some(mut pending) = pending else {
                return;
            };
            let runtime = weak.upgrade().unwrap();
            runtime
                .mutate_then_emit(false, |tree| {
                    let attempt = tree.attempt(&child).unwrap().attempt_id.clone();
                    tree.recover_host_decision(
                        &child,
                        &attempt,
                        RecoveryResolution::RecoveredWithoutTurn {
                            message: "quiescent".into(),
                        },
                    )
                    .map_err(|error| error.message)
                })
                .unwrap();
            // Release is published and settled, but the worker has not returned
            // its shared completion yet. Poll the expired joiner in this gap.
            let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
            assert!(matches!(
                pending.as_mut().poll(&mut cx),
                Poll::Ready(Err(_))
            ));
        }))
        .await;
    {
        let mut joiner = worker_joiner.lock().unwrap();
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(joiner.as_mut().unwrap().as_mut().poll(&mut cx).is_pending());
    }
    entered.notified().await;
    let completion = runtime.live.lock().await.retiring[&id].completion.clone();
    tokio::time::advance(HOST_RETIRE_JOIN_TIMEOUT).await;
    tokio::task::yield_now().await;
    runtime
        .read(|tree| {
            assert_eq!(
                tree.attempt(&id).unwrap().status,
                AttemptStatus::RecoveryRequired
            );
        })
        .unwrap();
    finish.notify_one();
    completion.await.unwrap();
    runtime
        .read(|tree| {
            assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Failed);
            assert_eq!(
                tree.node(&id).unwrap().status,
                crate::protocol::NodeStatus::Failed
            );
        })
        .unwrap();
}

/// A completed child's release can join the runtime's own store-bounded
/// shutdown work (an in-flight store call, then the shutdown receipt).
/// A release slower than a cancel settle but within that bound is not a
/// failure: close waits for it, records no fault and keeps the terminal.
#[tokio::test(start_paused = true)]
async fn slow_store_bounded_release_completes_close_without_a_fault() {
    use crate::protocol::{AttemptStatus, NodeStatus, SpawnAgentRequest, SubmitResultRequest};
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap("slow-release", root.path().to_path_buf());
    let id = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.submit_result(
        &id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&id).unwrap();
    tree.settle_for_test();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        tree,
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let recoveries = Arc::new(AtomicUsize::new(0));
    let triggered = recoveries.clone();
    runtime
        .set_retirement_recovery(Arc::new(move || {
            triggered.fetch_add(1, Ordering::SeqCst);
        }))
        .await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        runtime
            .try_occupy_live(
                id.clone(),
                Box::new(BarrierChild {
                    panic_on_release: false,
                    failure: None,
                    turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let child = id.clone();
    let mut caller = tokio::spawn(async move { owner.cancel_live(&child).await });
    entered.notified().await;
    // Past a cancel settle plus process-exit release, within the
    // runtime's store-bounded shutdown.
    tokio::time::advance(
        plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT
            + HOST_SHUTDOWN_RELEASE_TIMEOUT
            + Duration::from_secs(13),
    )
    .await;
    tokio::task::yield_now().await;
    assert!(
        tokio::time::timeout(Duration::ZERO, &mut caller)
            .await
            .is_err(),
        "close waits for a release still within its bound"
    );
    runtime
        .read(|tree| {
            assert_eq!(tree.fault_reason(&id), None);
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Completed);
        })
        .unwrap();
    finish.notify_one();
    assert_eq!(caller.await.unwrap(), Ok(LiveCancelOutcome::Cancelled));
    assert_eq!(recoveries.load(Ordering::SeqCst), 0);
    runtime
        .read(|tree| {
            assert_eq!(tree.fault_reason(&id), None);
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Completed);
            assert_ne!(
                tree.attempt(&id).unwrap().status,
                AttemptStatus::RecoveryRequired
            );
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn fast_clean_retirement_does_not_trigger_recovery() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("clean-retirement", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let triggered = calls.clone();
    runtime
        .set_retirement_recovery(Arc::new(move || {
            triggered.fetch_add(1, Ordering::SeqCst);
        }))
        .await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    finish.notify_one();
    assert!(
        runtime
            .try_occupy_live(
                "root".into(),
                Box::new(BarrierChild {
                    panic_on_release: false,
                    failure: None,
                    turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    entered,
                    finish,
                })
            )
            .await
            .is_ok()
    );
    runtime.cancel_live("root").await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn late_retirement_clears_slot_and_publishes_locator() {
    late_retirement(None).await;
}

#[tokio::test]
async fn completed_terminal_does_not_hide_its_owners_release_failure() {
    use crate::protocol::{NodeStatus, SpawnAgentRequest, SubmitResultRequest};
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap("completed-release-failure", root.path().to_path_buf());
    let id = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.submit_result(
        &id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&id).unwrap();
    tree.settle_for_test();
    assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Completed);
    let runtime = Arc::new(ProductTopologyRuntime::new(
        tree,
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        runtime
            .try_occupy_live(
                id.clone(),
                Box::new(BarrierChild {
                    panic_on_release: false,
                    failure: Some("owner release failed".into()),
                    turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    finish.notify_one();
    assert!(matches!(
        runtime.cancel_live(&id).await,
        Err(LiveCancelError::OwnerFailure(_))
    ));
    runtime
        .read(|tree| {
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
            assert!(matches!(
                tree.recovery_need(&id).unwrap(),
                crate::tree::RecoveryNeed::Session { .. }
            ));
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn startup_join_deadline_records_its_stage() {
    use crate::protocol::SpawnAgentRequest;
    let root = tempfile::tempdir().unwrap();
    let runtime = ProductTopologyRuntime::new(
        bootstrap("startup-deadline-stage", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    );
    let id = runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map(|child| child.agent_id)
                .map_err(|error| error.message)
        })
        .unwrap();
    let startup = runtime.reserve_startup(&id).await.unwrap();
    startup.set_stage(ChildStartupStage::Releasing);
    assert_eq!(
        runtime.cancel_live(&id).await,
        Err(LiveCancelError::JoinExpired)
    );
    runtime
        .read(|tree| {
            let message = &tree.attempt(&id).unwrap().failure.as_ref().unwrap().message;
            assert!(message.contains("startup"), "{message}");
            assert!(message.contains("Releasing"), "{message}");
        })
        .unwrap();
    runtime
        .finish_startup(
            &id,
            &startup,
            LiveRelease {
                feed: None,
                locator: None,
                failure: None,
            },
        )
        .await;
}

#[tokio::test(start_paused = true)]
async fn owner_release_failure_fences_unstarted_followup() {
    use crate::protocol::{
        AttemptStatus, MessagePurpose, NodeStatus, SendAgentMessageRequest, SpawnAgentRequest,
        SubmitResultRequest,
    };
    const SESSION: &str = "00000000-0000-4000-8000-000000000100";
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("followup-release-failure", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let id = runtime
        .mutate_then_emit(false, |tree| {
            let id = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .unwrap()
                .agent_id;
            tree.record_child_session(&id,
                "00000000-0000-4000-8000-000000000100/00000000-0000-4000-8000-000000000200/00000000-0000-4000-8000-000000000300").unwrap();
            tree.fold_due(&id).unwrap();
            tree.submit_result(
                &id,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed(&id).unwrap();
            Ok(id)
        })
        .unwrap();
    let first = runtime
        .read(|tree| tree.attempt(&id).unwrap().attempt_id.clone())
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        runtime
            .try_occupy_live(
                id.clone(),
                Box::new(BarrierChild {
                    panic_on_release: false,
                    failure: Some("owner release failed".into()),
                    turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let child = id.clone();
    let cancel = tokio::spawn(async move { owner.cancel_live(&child).await });
    entered.notified().await;
    let mut opener = String::new();
    runtime
        .mutate_then_emit(false, |tree| {
            opener = tree
                .send_agent_message(
                    "root",
                    &SendAgentMessageRequest {
                        refresh: None,
                        agent_id: id.clone(),
                        message: "again".into(),
                        purpose: Some(MessagePurpose::Followup),
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .map_err(|error| error.message)?
                .message_id;
            assert_ne!(tree.attempt(&id).unwrap().attempt_id, first);
            assert!(tree.attempt(&id).unwrap().turn_id.is_none());
            let mut without_release_failure = tree.clone();
            assert!(
                without_release_failure
                    .withdraw_unstarted_followup(&id, &opener, SESSION, 0)
                    .is_ok()
            );
            Ok(())
        })
        .unwrap();
    assert!(runtime.reserve_startup(&id).await.is_err());
    finish.notify_one();
    assert!(matches!(
        cancel.await.unwrap(),
        Err(LiveCancelError::OwnerFailure(_))
    ));
    assert!(!runtime.live.lock().await.contains_key(&id));
    runtime
        .mutate_then_emit(false, |tree| {
            assert!(
                tree.withdraw_unstarted_followup(&id, &opener, SESSION, 0)
                    .is_err(),
                "owner evidence must prevent silent rollback"
            );
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
            let attempt = tree.attempt(&id).unwrap();
            assert_eq!(attempt.status, AttemptStatus::RecoveryRequired);
            assert_eq!(attempt.failure.as_ref().unwrap().kind, "session_retirement");
            assert!(
                attempt
                    .failure
                    .as_ref()
                    .unwrap()
                    .message
                    .contains("owner release failed")
            );
            assert!(matches!(
                tree.recovery_need(&id).unwrap(),
                crate::tree::RecoveryNeed::Session { .. }
            ));
            tree.record_host_materialization(&id)
                .and_then(|value| tree.fold_due(&id).map(|_| value))
                .map_err(|error| error.message)?;
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
            assert!(
                tree.attempt(&id)
                    .unwrap()
                    .failure
                    .as_ref()
                    .unwrap()
                    .message
                    .contains("owner release failed")
            );
            Ok(())
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_failing_retirement_clears_slot_and_publishes_locator() {
    late_retirement(Some("injected cleanup failure".into())).await;
}

async fn late_retirement(failure: Option<String>) {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("late-retirement", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let recoveries = Arc::new(AtomicUsize::new(0));
    let triggered = recoveries.clone();
    runtime
        .set_retirement_recovery(Arc::new(move || {
            triggered.fetch_add(1, Ordering::SeqCst);
        }))
        .await;
    runtime
        .try_occupy_live(
            "root".into(),
            Box::new(BarrierChild {
                panic_on_release: false,
                failure: failure.clone(),
                turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                entered: entered.clone(),
                finish: finish.clone(),
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("occupied"));
    let owner = runtime.clone();
    let caller = tokio::spawn(async move { owner.cancel_live("root").await });
    entered.notified().await;
    let completion = runtime.live.lock().await.retiring["root"]
        .completion
        .clone();
    tokio::time::advance(HOST_RETIRE_JOIN_TIMEOUT + Duration::from_secs(1)).await;
    assert!(caller.await.unwrap().is_err(), "joiner is bounded");
    assert!(
        runtime.live.lock().await.contains_key("root"),
        "release still owns slot"
    );
    finish.notify_one();
    let result = completion.await;
    assert!(
        recoveries.load(Ordering::SeqCst) >= 2,
        "both the deadline and the returned late release must trigger recovery"
    );
    assert_eq!(
        result.is_err(),
        failure.is_some(),
        "late release was abandoned: {result:?}"
    );
    assert!(!runtime.live.lock().await.contains_key("root"));
    runtime
        .read(|tree| {
            assert_eq!(tree.node("root").unwrap().session_locator.as_deref(), None);
            assert_eq!(tree.revision, tree.stored_revision);
        })
        .unwrap();
}

#[test]
fn retirement_release_is_not_wrapped_in_timeout() {
    let source = include_str!("registry.rs");
    let worker = source
        .split("let task = tokio::spawn(async move {")
        .nth(1)
        .unwrap()
        .split("let retirement = Retirement {")
        .next()
        .unwrap();
    assert!(!worker.contains("tokio::time::timeout("));
}

#[tokio::test]
async fn cancel_reservation_excludes_reoccupation_until_locator_publication() {
    use std::str::FromStr;
    let old = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let new = TurnId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("retiring", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let child = |turn| {
        Box::new(BarrierChild {
            panic_on_release: false,
            failure: None,
            turn,
            entered: entered.clone(),
            finish: finish.clone(),
        }) as Box<dyn LiveChildHandle>
    };
    assert!(
        runtime
            .try_occupy_live("root".into(), child(old))
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let cancel = tokio::spawn(async move { owner.cancel_live("root").await });
    entered.notified().await;
    assert!(
        runtime.live.try_lock().is_ok(),
        "release must not hold the map mutex"
    );
    assert!(
        runtime
            .try_occupy_live("root".into(), child(new))
            .await
            .is_err()
    );
    let mut overlapping = Box::pin(runtime.cancel_live("root"));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(
            overlapping.as_mut().poll(cx).is_pending()
        ))
        .await
    );
    cancel.abort();
    assert!(cancel.await.unwrap_err().is_cancelled());
    finish.notify_one();
    assert_eq!(overlapping.await.unwrap(), LiveCancelOutcome::Cancelled);
    runtime
        .read(|tree| {
            assert_eq!(tree.node("root").unwrap().session_locator.as_deref(), None);
            assert_eq!(tree.revision, tree.stored_revision);
        })
        .unwrap();
    assert!(
        runtime
            .try_occupy_live("root".into(), child(new))
            .await
            .is_ok()
    );
    let mut live = runtime.live.lock().await;
    assert!(
        live.start_retire(
            runtime.live.clone(),
            runtime.driver.clone(),
            "root",
            Some(old),
            false
        )
        .is_none(),
        "old waiter cannot take replacement"
    );
    assert_eq!(live.get("root").unwrap().turn_id(), new);
}
#[tokio::test]
async fn abandoned_caller_still_records_panicking_release_fault() {
    use std::str::FromStr;
    let root = tempfile::tempdir().unwrap();
    let store = memory_store();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("panicking-retirement", root.path().to_path_buf()),
        store.clone(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent(
                "root",
                &crate::protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    assert!(
        runtime
            .try_occupy_live(
                "c1".into(),
                Box::new(BarrierChild {
                    panic_on_release: true,
                    failure: None,
                    turn,
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let caller = tokio::spawn(async move { owner.cancel_live("c1").await });
    entered.notified().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    // Observe the owned completion without starting a second cancellation.
    let completion = runtime
        .live
        .lock()
        .await
        .retiring
        .get("c1")
        .unwrap()
        .completion
        .clone();
    finish.notify_one();
    let failure = completion.await.unwrap_err();
    assert_eq!(failure, "child retirement worker panicked");
    assert!(!runtime.live.lock().await.contains_key("c1"));
    runtime
        .read(|tree| {
            assert_eq!(
                tree.node("c1").unwrap().status,
                crate::protocol::NodeStatus::RecoveryRequired
            );
            assert_eq!(tree.fault_reason("c1"), Some(failure.as_str()));
        })
        .unwrap();
    let persisted = store.load("panicking-retirement").unwrap().unwrap();
    assert_eq!(
        persisted.rows().node("c1").unwrap().status,
        crate::protocol::NodeStatus::RecoveryRequired
    );
    assert_eq!(
        persisted
            .rows()
            .attempt("c1")
            .unwrap()
            .failure
            .as_ref()
            .unwrap()
            .message,
        failure
    );
    assert_eq!(
        runtime.cancel_live("c1").await.unwrap(),
        LiveCancelOutcome::NotLive
    );
}

#[tokio::test]
async fn shutdown_joins_existing_retirement_and_concurrent_active_release() {
    use std::str::FromStr;
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("shutdown-retiring", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let child_id = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &crate::protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let second_entered = Arc::new(tokio::sync::Notify::new());
    let second_finish = Arc::new(tokio::sync::Notify::new());
    for (id, entered, finish) in [
        ("root".to_owned(), entered.clone(), finish.clone()),
        (
            child_id.clone(),
            second_entered.clone(),
            second_finish.clone(),
        ),
    ] {
        assert!(
            runtime
                .try_occupy_live(
                    id,
                    Box::new(BarrierChild {
                        panic_on_release: false,
                        failure: None,
                        turn,
                        entered,
                        finish,
                    })
                )
                .await
                .is_ok()
        );
    }
    let owner = runtime.clone();
    let cancel = tokio::spawn(async move { owner.cancel_live("root").await });
    entered.notified().await;
    let mut shutdown = Box::pin(shutdown_runtime(runtime.clone()));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(shutdown.as_mut().poll(cx).is_pending()))
            .await
    );
    second_entered.notified().await;
    second_finish.notify_one();
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(shutdown.as_mut().poll(cx).is_pending()))
            .await
    );
    assert!(runtime.live.try_lock().is_ok());
    finish.notify_one();
    shutdown.await.unwrap();
    assert_eq!(cancel.await.unwrap().unwrap(), LiveCancelOutcome::Cancelled);
    assert!(runtime.live.lock().await.is_empty());
    runtime
        .read(|tree| {
            for id in ["root", &child_id] {
                assert_eq!(tree.node(id).unwrap().session_locator.as_deref(), None);
            }
            assert_eq!(tree.revision, tree.stored_revision);
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_deadline_bounds_joiners_but_keeps_release_owner() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("shutdown-stuck", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        runtime
            .try_occupy_live(
                "root".into(),
                Box::new(BarrierChild {
                    panic_on_release: false,
                    failure: None,
                    turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let cancel = tokio::spawn(async move { owner.cancel_live("root").await });
    entered.notified().await;
    let completion = runtime.live.lock().await.retiring["root"]
        .completion
        .clone();
    let result = shutdown_runtime(runtime.clone()).await.unwrap_err();
    assert_eq!(result.child_failures.len(), 1);
    assert!(runtime.live.lock().await.contains_key("root"));
    finish.notify_one();
    completion.await.expect("shutdown must not abandon release");
    cancel.await.unwrap().unwrap();
    assert!(!runtime.live.lock().await.contains_key("root"));
    runtime
        .read(|tree| {
            assert_eq!(tree.node("root").unwrap().session_locator.as_deref(), None);
        })
        .unwrap();
}

#[tokio::test]
async fn refused_retirement_publication_clears_the_slot_but_keeps_fault() {
    use std::str::FromStr;
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("retirement-publish-refused", root.path().to_path_buf()),
        Arc::new(RefusingStore),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        runtime
            .try_occupy_live(
                "root".into(),
                Box::new(BarrierChild {
                    panic_on_release: false,
                    failure: None,
                    turn: TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let cancel = tokio::spawn(async move { owner.cancel_live("root").await });
    entered.notified().await;
    finish.notify_one();
    let failure = cancel.await.unwrap().unwrap_err();
    assert_eq!(
        runtime.cancel_live("root").await.unwrap(),
        LiveCancelOutcome::NotLive
    );
    assert!(!runtime.live.lock().await.contains_key("root"));
    runtime
        .read(|tree| {
            assert_eq!(
                tree.fault_reason("root"),
                Some(failure.to_string().as_str())
            );
            assert_eq!(
                tree.node("root").unwrap().status,
                crate::protocol::NodeStatus::Running
            );
            assert_ne!(tree.revision, tree.stored_revision);
        })
        .unwrap();
}
#[tokio::test]
async fn batch_cancel_starts_all_releases_before_waiting_and_joins_overlap() {
    use std::str::FromStr;
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        bootstrap("batch-cancel", root.path().to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let child = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &crate::protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    let ids = vec![child, "root".to_owned()];
    let mut barriers = Vec::new();
    for id in &ids {
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        assert!(
            runtime
                .try_occupy_live(
                    id.clone(),
                    Box::new(BarrierChild {
                        panic_on_release: false,
                        failure: None,
                        turn: TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
                        entered: entered.clone(),
                        finish: finish.clone(),
                    })
                )
                .await
                .is_ok()
        );
        barriers.push((entered, finish));
    }
    let mut batch = Box::pin(runtime.cancel_live_many(&ids));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(batch.as_mut().poll(cx).is_pending()))
            .await
    );
    for (entered, _) in &barriers {
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
    }
    let mut overlap = Box::pin(runtime.cancel_live(&ids[0]));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(overlap.as_mut().poll(cx).is_pending()))
            .await
    );
    barriers[1].1.notify_one();
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(batch.as_mut().poll(cx).is_pending()))
            .await
    );
    barriers[0].1.notify_one();
    let report = batch.await;
    assert!(report.is_clean());
    assert_eq!(report.cancelled, ids);
    assert_eq!(overlap.await.unwrap(), LiveCancelOutcome::Cancelled);
    assert!(runtime.live.lock().await.is_empty());
}

fn recovery_runtime(name: &str, root: &Path) -> Arc<ProductTopologyRuntime> {
    Arc::new(ProductTopologyRuntime::new(
        bootstrap(name, root.to_path_buf()),
        memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ))
}

/// A request while a pass runs is not dropped: the running pass is told to
/// run once more, and only then gives up the slot.
#[tokio::test]
async fn recovery_pass_request_while_running_reruns_it() {
    let root = tempfile::tempdir().unwrap();
    let runtime = recovery_runtime("recovery-pass-rerun", root.path());
    let gate = Arc::new(tokio::sync::Notify::new());
    let runs = Arc::new(AtomicUsize::new(0));
    let (pass_gate, pass_runs, pass_runtime) = (gate.clone(), runs.clone(), runtime.clone());
    let started = runtime.start_recovery_pass(move || {
        tokio::spawn(async move {
            loop {
                pass_gate.notified().await;
                pass_runs.fetch_add(1, Ordering::SeqCst);
                if !pass_runtime.finish_recovery_pass() {
                    break;
                }
            }
        })
    });
    assert_eq!(started, RecoveryPassRequest::Started);
    let second = runtime.start_recovery_pass(|| unreachable!("a running pass reruns"));
    assert_eq!(second, RecoveryPassRequest::Rerun);
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runs.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        runtime.recovery_pass_running(),
        "the queued rerun holds the slot"
    );
    gate.notify_one();
    let task = runtime.recovery_pass.lock().unwrap().task.take().unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert!(!runtime.recovery_pass_running());
}

#[tokio::test]
async fn recovery_pass_request_after_close_starts_nothing() {
    let root = tempfile::tempdir().unwrap();
    let runtime = recovery_runtime("recovery-pass-closed", root.path());
    runtime.close_for_shutdown();
    let request = runtime.start_recovery_pass(|| unreachable!("a closed hub starts nothing"));
    assert_eq!(request, RecoveryPassRequest::Closed);
    assert!(!runtime.recovery_pass_running());
}

/// A pass that never finishes gets its own bounded wait, then is aborted
/// and joined briefly, so shutdown still ends.
#[tokio::test]
async fn shutdown_aborts_a_recovery_pass_that_overruns() {
    let root = tempfile::tempdir().unwrap();
    let runtime = recovery_runtime("recovery-pass-shutdown", root.path());
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
    let request = runtime.start_recovery_pass(move || {
        tokio::spawn(async move {
            let _dropped = dropped_tx;
            std::future::pending::<()>().await;
        })
    });
    assert_eq!(request, RecoveryPassRequest::Started);
    let begun = std::time::Instant::now();
    // The verdict on the tree's own teardown is not this test's subject.
    let _ = tokio::time::timeout(Duration::from_secs(4), shutdown_runtime(runtime.clone()))
        .await
        .expect("shutdown must not wait out a stuck recovery pass");
    assert!(begun.elapsed() < HOST_SHUTDOWN_RELEASE_TIMEOUT + Duration::from_millis(1500));
    assert!(
        dropped_rx.await.is_err(),
        "the aborted pass dropped its state"
    );
    assert!(runtime.is_closed());
}
