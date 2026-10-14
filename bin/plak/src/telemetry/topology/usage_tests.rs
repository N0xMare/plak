#![allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]

use super::*;
use crate::home::paths::PlakPaths;
use crate::telemetry_store::{TelemetryItem, TelemetryStore};
use plakous::agent::{CommandId, TurnUsage};
use plakous::model::{FinishReason, Usage};
use plakous::sessions::feed::{FeedCommandKind, FeedFact, FeedHeader, FeedRecord, FeedTurnOutcome};
use plakous_hosted::agent_tree::{
    AgentCommand, AgentCommandKind, MemoryTreeStore, ProductAgentTree, TreeFeed, TreeFeedReads,
    TreeStore,
};
use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

struct ChildUsageFixture {
    _workspace: tempfile::TempDir,
    runtime: Arc<plakous_hosted::topology_runtime::ProductTopologyRuntime>,
    child: String,
    feed: TreeFeed,
    receiver: std::sync::mpsc::Receiver<TelemetryItem>,
}

impl ChildUsageFixture {
    fn new(store: Arc<dyn TreeStore>) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let tree_id = uuid::Uuid::new_v4().to_string();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id.clone(),
            "root",
            "owner",
            workspace.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        tree.record_command(AgentCommand::intent(
            "child-usage",
            &child.agent_id,
            Some(child.attempt_id.clone()),
            AgentCommandKind::Start,
            "assignment",
            Vec::new(),
        ))
        .unwrap();
        tree.acknowledge_revision(store.save(&tree).unwrap());
        let (writer, receiver) = TelemetryWriter::detached(8);
        let observer = ProductTopologyObserver::port(Some(writer)).unwrap();
        let runtime = plakous_hosted::topology_runtime::insert_root_with(
            tree_id,
            tree,
            store,
            Some(observer),
        )
        .unwrap();
        let session = "00000000-0000-4000-8000-0000000000c1";
        let turn = "00000000-0000-4000-8000-000000001001".parse().unwrap();
        let header: FeedHeader = serde_json::from_value(serde_json::json!({
        "session_id": session, "seq": 1,
        "branch_id": "00000000-0000-4000-8000-00000000000b",
        "committed_at_ms": 1000,
        "change": {"created": {"genesis": "00000000-0000-4000-8000-000000003001", "revision": 1}}
    }))
    .unwrap();
        let usage = TurnUsage::new(Usage::new(123, 45, 6, 7, 8), 1, 1, 0).unwrap();
        let feed = TreeFeed {
            session_id: session.to_owned(),
            retained_from: 0,
            below_retained: None,
            heads: Vec::new(),
            head_turns: std::collections::BTreeMap::new(),
            reads: TreeFeedReads::default(),
            records: vec![FeedRecord::from_store(
                header,
                vec![
                    FeedFact::CommandAccepted {
                        command_id: CommandId::new("child-usage").unwrap(),
                        kind: FeedCommandKind::Root {
                            origin: plakous::model::TurnOrigin::UserPrompt,
                        },
                        turn_id: Some(turn),
                        sequence: 1,
                        operation_id: None,
                    },
                    FeedFact::TurnTerminal {
                        turn_id: turn,
                        command_id: CommandId::new("child-usage").unwrap(),
                        outcome: FeedTurnOutcome::Completed {
                            finish: FinishReason::Stop,
                        },
                        closed_exchange: true,
                        steering_settled_unapplied: false,
                        usage,
                    },
                ],
            )],
        };
        Self {
            _workspace: workspace,
            runtime,
            child: child.agent_id,
            feed,
            receiver,
        }
    }

    fn project(&self) -> Result<plakous_hosted::agent_tree::TreeProjection, String> {
        self.runtime.mutate_then_emit(false, |tree| {
            self.feed
                .apply(tree, &self.child)
                .map_err(|error| error.message)
        })
    }
}

fn turn_usage() -> TurnUsage {
    TurnUsage::new(Usage::new(123, 45, 6, 7, 8), 1, 1, 0).unwrap()
}

#[derive(Default)]
struct FailingStore {
    inner: MemoryTreeStore,
    fail: std::sync::atomic::AtomicBool,
}

impl TreeStore for FailingStore {
    fn exists(&self, id: &str) -> Result<bool, plakous_hosted::agent_tree::TreeStoreError> {
        self.inner.exists(id)
    }

    fn ids(&self) -> Result<Vec<String>, plakous_hosted::agent_tree::TreeStoreError> {
        self.inner.ids()
    }

    fn load(
        &self,
        id: &str,
    ) -> Result<
        Option<plakous_hosted::agent_tree::PersistedTree>,
        plakous_hosted::agent_tree::TreeStoreError,
    > {
        self.inner.load(id)
    }

    fn save(
        &self,
        tree: &ProductAgentTree,
    ) -> Result<plakous_hosted::agent_tree::Revision, plakous_hosted::agent_tree::TreeStoreError>
    {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(plakous_hosted::agent_tree::TreeStoreError::unavailable(
                "injected save failure",
            ));
        }
        self.inner.save(tree)
    }
}

#[test]
fn failed_child_usage_save_retries_without_reload_and_emits_once() {
    let store = Arc::new(FailingStore::default());
    let fixture = ChildUsageFixture::new(store.clone());
    let cursor = fixture
        .runtime
        .read(|tree| tree.feed_cursor(&fixture.feed.session_id))
        .unwrap();
    store.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        fixture
            .project()
            .unwrap_err()
            .contains("injected save failure")
    );
    assert!(fixture.receiver.try_recv().is_err());
    assert_eq!(
        fixture
            .runtime
            .read(|tree| tree.feed_cursor(&fixture.feed.session_id))
            .unwrap(),
        cursor
    );
    store.fail.store(false, std::sync::atomic::Ordering::SeqCst);
    fixture.runtime.mutate_then_emit(false, |_| Ok(())).unwrap();
    assert!(fixture.receiver.try_recv().is_err());
    assert_eq!(fixture.project().unwrap().applied, 1);
    assert!(
        matches!(fixture.receiver.try_recv(), Ok(TelemetryItem::Product(ProductObservation::TurnUsage { usage, .. })) if usage == turn_usage())
    );
    fixture.project().unwrap();
    assert!(fixture.receiver.try_recv().is_err());
}

#[test]
fn child_projected_usage_is_one_keyed_row_across_reprojection_and_rebuild() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let mut telemetry = TelemetryStore::open(&paths).unwrap();
    let fixture = ChildUsageFixture::new(Arc::new(MemoryTreeStore::new()));
    let project = || fixture.project().unwrap();
    assert_eq!(project().applied, 1);
    let item = fixture
        .receiver
        .try_recv()
        .expect("the committed child terminal emits usage");
    let TelemetryItem::Product(ProductObservation::TurnUsage {
        turn_id,
        usage: emitted,
        ..
    }) = item
    else {
        panic!("projected turn usage");
    };
    assert_eq!(turn_id.to_string(), "00000000-0000-4000-8000-000000001001");
    assert_eq!(emitted, turn_usage());
    telemetry.write_batch(&[item]).unwrap();
    assert_eq!(project().applied, 0);
    assert!(fixture.receiver.try_recv().is_err());
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.clear_feed_views();
            Ok(())
        })
        .unwrap();
    assert_eq!(project().applied, 1);
    telemetry
        .write_batch(&[fixture.receiver.try_recv().unwrap()])
        .unwrap();
    assert_eq!(telemetry.count_rows("turns"), 1);
    assert_eq!(
        telemetry.query_i64("SELECT sum(input_tokens) FROM turns"),
        Some(123)
    );
    assert_eq!(
        telemetry.query_i64("SELECT sum(output_tokens) FROM turns"),
        Some(45)
    );
    assert!(fixture.receiver.try_recv().is_err());
}
