use crate::agent_tree_protocol::{NodeStatus, SpawnAgentRequest};
use crate::runtime::test_kit::BlockOn as _;

#[test]
fn a_failed_recovery_join_does_not_retrack_its_spent_handle() {
    let root = tempfile::tempdir().unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let runtime = crate::runtime::test_kit::install_idle_topology_hub(&tree_id, root.path());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let recovery = runtime
                .driver()
                .tasks()
                .spawn(async { panic!("failed recovery fixture") });
            assert!(runtime.track_recovery(recovery));
            assert!(
                super::mark_root_interrupted_for_run(&tree_id)
                    .await
                    .is_err()
            );
            super::mark_root_interrupted_for_run(&tree_id)
                .await
                .unwrap();
            crate::topology_runtime::force_release(&tree_id).await;
        })
        .unwrap();
}

#[test]
fn idle_stop_joins_child_recovery_before_acquiring_process_stop_leases() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let root = tempfile::tempdir().unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let runtime = crate::runtime::test_kit::install_idle_topology_hub(&tree_id, root.path());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let released = Arc::new(AtomicBool::new(false));
            let (release, waiting) = tokio::sync::oneshot::channel();
            let finished = Arc::clone(&released);
            let recovery = runtime.driver().tasks().spawn(async move {
                waiting.await.unwrap();
                finished.store(true, Ordering::SeqCst);
            });
            assert!(runtime.track_recovery(recovery));
            super::signal_root_stopped(&tree_id).await.unwrap();
            assert!(!released.load(Ordering::SeqCst));
            let mut stop = Box::pin(super::mark_root_interrupted_for_run(&tree_id));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut stop)
                    .await
                    .is_err(),
                "the idle stop must wait for recovery to release its child session"
            );
            release.send(()).unwrap();
            stop.await.unwrap();
            assert!(released.load(Ordering::SeqCst));
        })
        .unwrap();
}

#[test]
fn a_hung_recovery_does_not_prevent_working_children_from_being_stopped() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let root = tempfile::tempdir().unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let runtime = crate::runtime::test_kit::install_idle_topology_hub(&tree_id, root.path());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let child = runtime
                .mutate_then_emit(false, |tree| {
                    let child = tree
                        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                        .unwrap();
                    tree.set_active_turn(&child.agent_id, true).unwrap();
                    Ok(child.agent_id)
                })
                .await
                .unwrap();
            let signalled = Arc::new(AtomicBool::new(false));
            let released = Arc::new(AtomicBool::new(false));
            let finish = Arc::new(tokio::sync::Notify::new());
            assert!(
                runtime
                    .try_occupy_live(
                        child.clone(),
                        Box::new(DelayedChild(
                            released.clone(),
                            finish.clone(),
                            Some(signalled.clone())
                        ))
                    )
                    .await
                    .is_ok()
            );
            let (release, waiting) = tokio::sync::oneshot::channel();
            assert!(
                runtime.track_recovery(runtime.driver().tasks().spawn(async move {
                    let _ = waiting.await;
                }))
            );
            let error = super::mark_root_interrupted_for_run(&tree_id)
                .await
                .unwrap_err();
            assert!(error.contains("child recovery exceeded"), "{error}");
            assert!(runtime.recovery_running());
            assert_eq!(
                runtime
                    .read(|tree| tree.node(&child).unwrap().status)
                    .unwrap(),
                NodeStatus::Interrupted
            );
            assert_eq!(
                runtime
                    .read(|tree| tree.interrupt_cause_of(&child))
                    .unwrap(),
                Some(crate::agent_tree::InterruptCause::Stopped)
            );
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while !signalled.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                !released.load(Ordering::SeqCst),
                "the signal path must not join retirement"
            );
            finish.notify_one();
            release.send(()).unwrap();
            crate::topology_runtime::force_release(&tree_id).await;
        })
        .unwrap();
}

struct DelayedChild(
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::sync::Arc<tokio::sync::Notify>,
    Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
);
impl crate::topology_runtime::LiveChildHandle for DelayedChild {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        None
    }
    fn turn_id(&self) -> plakous::model::TurnId {
        "00000000-0000-0000-0000-000000000001".parse().unwrap()
    }
    fn turn_control(&self) -> plakous::agent::TurnControl {
        panic!("unused")
    }
    fn follow_up(
        &self,
        _: plakous::agent::CommandId,
        _: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Ok(()) })
    }

    fn follow_up_barrier(&self) -> Option<std::sync::Arc<tokio::sync::RwLock<()>>> {
        None
    }
    fn restart(
        &mut self,
        _: crate::agent_tree_tools::TreeDriver,
        _: plakous::agent::CommandId,
        _: String,
        _delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>,
    > {
        Box::pin(async { Err("unused".into()) })
    }
    fn release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        self.cancel_and_release(driver, None)
    }
    fn cancel_and_release(
        self: Box<Self>,
        _driver: crate::agent_tree_tools::TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        if let Some(signalled) = &self.2 {
            signalled.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(dispatched) = dispatched {
            let _ = dispatched.send(());
        }
        Box::pin(async move {
            self.1.notified().await;
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            crate::topology_runtime::LiveRelease::default()
        })
    }
}

#[test]
fn root_interrupt_signals_children_and_shutdown_joins_their_release() {
    let root = tempfile::tempdir().unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let runtime = crate::runtime::test_kit::install_idle_topology_hub(&tree_id, root.path());
    let child = runtime
        .mutate_then_emit(false, |tree| {
            let id = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .unwrap()
                .agent_id;
            tree.set_active_turn(&id, true).unwrap();
            Ok(id)
        })
        .block()
        .unwrap();
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finish = std::sync::Arc::new(tokio::sync::Notify::new());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    assert!(
        executor
            .block_on(runtime.try_occupy_live(
                child.clone(),
                Box::new(DelayedChild(cancelled.clone(), finish.clone(), None))
            ))
            .unwrap()
            .is_ok()
    );
    let interrupted = executor
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                super::mark_root_interrupted(&tree_id),
            )
            .await
        })
        .unwrap();
    assert!(
        interrupted.is_ok(),
        "root interrupt must return before blocked release settles"
    );
    assert_eq!(interrupted.unwrap().unwrap(), 1);
    assert!(
        !cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "interrupt returns before release settles"
    );
    finish.notify_one();
    let released = executor
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                crate::topology_runtime::force_release(&tree_id),
            )
            .await
        })
        .unwrap();
    assert!(
        released.is_ok(),
        "shutdown must join the signalled release within its bound"
    );
    assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
}

/// A root cancel interrupts the children that are running, so they stop
/// spending, and reports how many; a tree with none says zero.
#[test]
fn a_root_interrupt_stops_running_children() {
    let root = tempfile::tempdir().unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let runtime = crate::runtime::test_kit::install_idle_topology_hub(&tree_id, root.path());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    assert_eq!(
        executor
            .block_on(super::mark_root_interrupted(&tree_id))
            .unwrap()
            .unwrap(),
        0
    );
    let children = runtime
        .mutate_then_emit(false, |tree| {
            let mut ids = Vec::new();
            for _ in 0..2 {
                let id = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .unwrap()
                    .agent_id;
                tree.set_active_turn(&id, true).unwrap();
                ids.push(id);
            }
            Ok(ids)
        })
        .block()
        .unwrap();
    assert_eq!(
        executor
            .block_on(super::mark_root_interrupted(&tree_id))
            .unwrap()
            .unwrap(),
        2
    );
    for id in &children {
        assert_eq!(
            runtime.read(|tree| tree.node(id).unwrap().status).unwrap(),
            NodeStatus::Interrupted
        );
    }
    plakous::application::ApplicationExecutor::new()
        .unwrap()
        .block_on(crate::topology_runtime::force_release(&tree_id))
        .unwrap();
}

/// A tree store whose capacity the test switches.
struct SwitchedCapacity {
    inner: crate::agent_tree::MemoryTreeStore,
    full: std::sync::atomic::AtomicBool,
}

impl crate::agent_tree::TreeStore for SwitchedCapacity {
    fn exists(&self, id: &str) -> Result<bool, crate::agent_tree::TreeStoreError> {
        self.inner.exists(id)
    }
    fn ids(&self) -> Result<Vec<String>, crate::agent_tree::TreeStoreError> {
        self.inner.ids()
    }
    fn load(
        &self,
        id: &str,
    ) -> Result<Option<crate::agent_tree::PersistedTree>, crate::agent_tree::TreeStoreError> {
        self.inner.load(id)
    }
    fn save(
        &self,
        tree: &crate::agent_tree::ProductAgentTree,
        claim: &crate::agent_tree::CustodyClaim,
    ) -> Result<crate::agent_tree::Revision, crate::agent_tree::TreeStoreError> {
        if self.full.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::agent_tree::TreeStoreError::storage_full());
        }
        self.inner.save(tree, claim)
    }

    fn custody_port(&self) -> &dyn crate::agent_tree::TreeCustody {
        self.inner.custody_port()
    }
}

/// A root publication on a full tree store parks in its caller, then
/// publishes once capacity returns, or stops when the host shuts down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_publication_parks_on_a_full_tree_store_until_released_or_shut_down() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let store = std::sync::Arc::new(SwitchedCapacity {
        inner: crate::agent_tree::MemoryTreeStore::new(),
        full: std::sync::atomic::AtomicBool::new(false),
    });
    let runtime = crate::topology_runtime::insert_root_with(
        tree_id.clone(),
        crate::topology_runtime::test_support::bootstrap(&tree_id, root.path().to_path_buf()),
        store.clone(),
        None,
    )
    .await
    .unwrap();
    let host = crate::agent_tree_tools::HostShutdown::new();
    runtime.driver().bind_host_shutdown(host.clone());
    let spawn = |runtime: std::sync::Arc<crate::topology_runtime::ProductTopologyRuntime>| {
        tokio::spawn(async move {
            runtime
                .mutate_then_emit(false, |tree| {
                    tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                        .map(|_| ())
                        .map_err(|error| error.message)
                })
                .await
        })
    };
    store.full.store(true, Ordering::Release);
    let released = spawn(runtime.clone());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!released.is_finished(), "the publication parks");
    store.full.store(false, Ordering::Release);
    released.await.unwrap().unwrap();
    store.full.store(true, Ordering::Release);
    let shut = spawn(runtime.clone());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!shut.is_finished(), "the publication parks again");
    host.signal();
    // A liveness bound only: generous, so load never trips it.
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(30), shut)
            .await
            .unwrap()
            .unwrap()
            .is_err(),
        "the host's shutdown ends the park without publishing"
    );
    store.full.store(false, Ordering::Release);
    crate::topology_runtime::force_release(&tree_id).await;
}
/// A model rebind that stays on its tree keeps that tree's roster: the
/// working set, every card and the bounds stay frozen, and only the root's
/// account, model and effort move. Both the hub and the headless path.
#[tokio::test]
async fn a_model_rebind_epoch_keeps_the_trees_roster() {
    use crate::agent_tree::{ProductAgentTree, TreeStore as _};
    for attached in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let tree_id = uuid::Uuid::new_v4().to_string();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id.as_str(),
            "root",
            "owner",
            root.path().to_path_buf(),
            "gpt-5.6-luna",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .unwrap();
        let rows = crate::runtime::test_kit::MemoryRows::new();
        let before = tree.current_epoch().clone();
        if attached {
            crate::topology_runtime::insert_root(tree_id.clone(), tree, None)
                .await
                .unwrap();
        } else {
            let acknowledged = rows.trees().save_in_custody(&tree).unwrap();
            tree.acknowledge_revision(acknowledged);
        }

        super::install_root_bind_epoch(
            &rows,
            &tree_id,
            "claude",
            "claude-opus-5-5",
            Some("high"),
            &|_| {},
        )
        .await
        .unwrap();

        let after = if attached {
            crate::topology_runtime::lookup(&tree_id)
                .unwrap()
                .read(|tree| tree.current_epoch().clone())
                .unwrap()
        } else {
            super::TreeRows::tree(&rows, &tree_id)
                .unwrap()
                .unwrap()
                .current_epoch()
                .clone()
        };
        assert_eq!(after.root_account, "claude", "attached={attached}");
        assert_eq!(after.root_tuple, "claude-opus-5-5");
        assert_eq!(after.root_effort.as_deref(), Some("high"));
        assert!(after.digest_matches());
        assert_eq!(
            after,
            before.with_root_bind("claude", "claude-opus-5-5", Some("high".to_owned()))
        );
        assert_eq!(after.working_set, before.working_set);
        assert_eq!(after.roster, before.roster);
        assert_eq!(after.templates_digest, before.templates_digest);
        if attached {
            crate::topology_runtime::force_release(&tree_id).await;
        }
    }
}

/// A /new session opens a tree of its own: with sub-agents off its source's
/// tree is not its tree, and once sub-agents are on it bootstraps a tree
/// under its own id. Only a rebind that stayed on its source's tree inherits
/// it.
#[test]
fn a_new_session_resolves_its_own_tree_and_a_staying_rebind_its_sources() {
    use super::durable_tree_id;
    use crate::agent_tree::{ProductAgentTree, TreeStore as _};
    use crate::agent_tree_protocol::WriterMode;
    use crate::session_store::SessionRecordStore as _;
    use crate::sessions::{
        SessionNode, SessionRecord, SessionWorkflowKind, projector::WorkflowRequest,
    };
    let root = tempfile::tempdir().unwrap();
    let rows = crate::runtime::test_kit::MemoryRows::new();
    let mut source = SessionRecord::new(root.path().to_path_buf());
    source
        .bind_plakous_locator(
            "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003"
                .parse()
                .unwrap(),
        )
        .unwrap();
    rows.records().save(&mut source).unwrap();
    let seed = |tree_id: &str| {
        let tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id,
            tree_id,
            "root",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let _revision = rows.trees().save_in_custody(&tree).unwrap();
    };
    seed(&source.id);
    let request = WorkflowRequest {
        operation_id: "plak:new:own-tree".into(),
        source_locator: source.plakous_locator.unwrap(),
        kind: SessionWorkflowKind::New,
    };
    let fresh = source.new_session_from_request(&request).unwrap();
    assert_eq!(fresh.node, SessionNode::genesis(&fresh.id));
    // Sub-agents off: nothing exists under its id, and it still does not
    // resolve its predecessor's tree.
    assert_eq!(durable_tree_id(&rows, &fresh), fresh.id);
    // Sub-agents on: its genesis bootstraps under that id, its own tree.
    seed(&fresh.id);
    assert_eq!(durable_tree_id(&rows, &fresh), fresh.id);
    assert_eq!(durable_tree_id(&rows, &source), source.id);

    // A rebind that stayed on its source's tree copies its node and
    // resolves the source's tree.
    let mut stayed = SessionRecord::new(root.path().to_path_buf());
    stayed.node = source.node.clone();
    stayed.source_product_session_id = Some(source.id.clone());
    stayed.source_workflow_operation_id = Some("plak:rebind:stayed".into());
    assert_eq!(durable_tree_id(&rows, &stayed), source.id);
}
