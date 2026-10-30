#![allow(clippy::unwrap_used, reason = "test assertions")]

use plakous_host_protocol::status::HostStatusDetail;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use crate::agent_tree::{
    AgentCommandKind, MemoryTreeStore, PersistedTree, ProductAgentTree, Revision, TreeStore,
    TreeStoreError,
};
use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, WriterMode};
use crate::runtime::test_support::{fresh_injected_attached_host, send_attached_action};
use crate::session_store::{MemoryStore, SessionRecordStore as _};
use crate::surface_protocol::{HostEvent, UserAction};

#[derive(Default)]
struct FullRootBatches {
    inner: MemoryTreeStore,
    full: AtomicBool,
    refused: AtomicUsize,
}

struct TreeFactory(Arc<FullRootBatches>);

impl crate::agent::TreeStoreFactory for TreeFactory {
    fn open(&self) -> crate::CliResult<Arc<dyn TreeStore>> {
        Ok(self.0.clone())
    }
}

impl TreeStore for FullRootBatches {
    fn exists(&self, id: &str) -> Result<bool, TreeStoreError> {
        self.inner.exists(id)
    }
    fn ids(&self) -> Result<Vec<String>, TreeStoreError> {
        self.inner.ids()
    }
    fn load(&self, id: &str) -> Result<Option<PersistedTree>, TreeStoreError> {
        self.inner.load(id)
    }
    fn save(&self, tree: &ProductAgentTree) -> Result<Revision, TreeStoreError> {
        if self.full.load(Ordering::Acquire)
            && tree
                .commands()
                .values()
                .any(|row| row.kind == AgentCommandKind::RootBatch)
        {
            assert!(
                crate::topology_runtime::lookup(&tree.tree_id).is_none(),
                "exercise the unattached branch"
            );
            self.refused.fetch_add(1, Ordering::AcqRel);
            return Err(TreeStoreError::storage_full());
        }
        self.inner.save(tree)
    }
}

#[test]
fn uncancelled_stored_only_root_batch_full_keeps_run_loop_alive_and_retries_once() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.surface = None;
    let records = Arc::new(MemoryStore::new());
    let item = crate::sessions::QueueItem::new("retry exactly once");
    config.session.pending_queue.push(item.clone());
    config.session.queue_drain_state = crate::sessions::QueueDrainState::RestoredPaused;
    records.save(&mut config.session).unwrap();
    let id = config.session.id.clone();
    let trees = Arc::new(FullRootBatches::default());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &id,
        "root",
        "owner",
        config.cwd.clone(),
        "selected",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    tree.submit_result(
        &child.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let revision = trees.save(&tree).unwrap();
    tree.acknowledge_revision(revision);
    let original = &config.state_roots;
    config.state_roots = crate::agent::ProductStateRoots::new(
        config.product_home.clone(),
        config.product_home.join("runtime.sqlite3"),
        config.product_home.join("state"),
        Arc::new(crate::agent::roots::doubles::NoTelemetry),
        Arc::new(TreeFactory(trees.clone())),
        original.roster_source().clone(),
        original.sidecars().clone(),
        original.openrouter_credentials(),
        Arc::new(crate::platform::UnavailableCredentials),
        records.clone(),
    )
    .unwrap();
    config.store = records.clone();
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    send_attached_action(&actions, UserAction::Steer("ready?".into()));
    loop {
        if matches!(events.recv_timeout(Duration::from_secs(15)).unwrap(), HostEvent::Status { ref detail, .. } if matches!(detail, Some(HostStatusDetail::NoActiveTurn)))
        {
            break;
        }
    }
    trees.full.store(true, Ordering::Release);
    assert!(
        !trees.storage_wait_cancelled(),
        "the store never signals operator cancellation"
    );
    send_attached_action(&actions, UserAction::ResumeQueueDrain);
    loop {
        let event = events.recv_timeout(Duration::from_secs(15)).unwrap();
        if matches!(event, HostEvent::Row(crate::sessions::TranscriptRow::Notice(ref row)) if row.text == "storage write unavailable; retained work and queued inputs remain available")
        {
            break;
        }
    }
    assert_eq!(trees.refused.load(Ordering::Acquire), 1);
    assert!(!worker.is_finished());
    assert!(!records.storage_wait_cancelled());
    assert_eq!(
        records.load(&id).unwrap().pending_queue,
        std::slice::from_ref(&item)
    );
    let stored = trees.load(&id).unwrap().unwrap();
    let stored = stored.rows();
    assert!(stored.commands().is_empty());
    assert_eq!(stored.pending_mail("root").len(), 1);
    send_attached_action(&actions, UserAction::Steer("still alive?".into()));
    loop {
        if matches!(events.recv_timeout(Duration::from_secs(15)).unwrap(), HostEvent::Status { ref detail, .. } if matches!(detail, Some(HostStatusDetail::NoActiveTurn)))
        {
            break;
        }
    }
    trees.full.store(false, Ordering::Release);
    send_attached_action(&actions, UserAction::ResumeQueueDrain);
    loop {
        if matches!(events.recv_timeout(Duration::from_secs(15)).unwrap(), HostEvent::TurnLifecycle(ref turn) if turn.phase == plakous_host_protocol::TurnPhase::Completed)
        {
            break;
        }
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let record = records.load(&id).unwrap();
    assert_eq!(record.root_turn_count, 1);
    assert_eq!(record.pending_queue, Vec::new());
    let stored = trees.load(&id).unwrap().unwrap();
    let stored = stored.rows();
    assert_eq!(stored.commands().len(), 1);
    assert!(
        stored
            .command(&format!("plak.queue.{}.{}", item.id, item.revision))
            .is_some()
    );
    assert_eq!(trees.refused.load(Ordering::Acquire), 1);
}
