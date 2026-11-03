#![allow(clippy::unwrap_used, reason = "test assertions")]
use crate::runtime::test_kit::BlockOn as _;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::agent_tree::{
    CustodyClaim, MemoryTreeStore, PersistedTree, ProductAgentTree, Revision, TreeCustody,
    TreeStore, TreeStoreError,
};
use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, WriterMode};
use crate::runtime::topology_state::{TopologyRows, TreeRows};
use crate::session_store::{MemoryStore, RecordStoreError, SessionRecordStore as _};
use crate::sessions::{QueueItem, SessionRecord};

#[derive(Default)]
struct CapacityStore {
    inner: MemoryTreeStore,
    full: AtomicBool,
}

impl TreeStore for CapacityStore {
    fn exists(&self, id: &str) -> Result<bool, TreeStoreError> {
        self.inner.exists(id)
    }
    fn ids(&self) -> Result<Vec<String>, TreeStoreError> {
        self.inner.ids()
    }
    fn load(&self, id: &str) -> Result<Option<PersistedTree>, TreeStoreError> {
        self.inner.load(id)
    }
    fn save(
        &self,
        tree: &ProductAgentTree,
        claim: &CustodyClaim,
    ) -> Result<Revision, TreeStoreError> {
        if self.full.load(Ordering::SeqCst) {
            return Err(TreeStoreError::storage_full());
        }
        self.inner.save(tree, claim)
    }

    fn custody_port(&self) -> &dyn TreeCustody {
        self.inner.custody_port()
    }
}

#[derive(Default)]
struct Rows {
    trees: Arc<CapacityStore>,
    records: MemoryStore,
}

impl TreeRows for Rows {
    fn tree(&self, id: &str) -> Result<Option<ProductAgentTree>, String> {
        self.trees
            .load(id)
            .map(|tree| tree.map(ProductAgentTree::reopen))
            .map_err(|error| error.detail)
    }
    fn tree_exists(&self, id: &str) -> Result<bool, String> {
        self.trees.exists(id).map_err(|error| error.detail)
    }
    fn save_tree(&self, tree: &mut ProductAgentTree) -> Result<(), crate::runtime::TreeWriteError> {
        let revision = self.trees.save_in_custody(tree)?;
        tree.acknowledge_revision(revision);
        Ok(())
    }
}

impl TopologyRows for Rows {
    fn session(&self, id: &str) -> Result<SessionRecord, String> {
        self.records.load(id)
    }
}

fn result_tree(session: &SessionRecord) -> ProductAgentTree {
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &session.id,
        "root",
        "owner",
        session.workspace_root.clone(),
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
    tree
}

#[tokio::test]
async fn root_batch_capacity_refusal_is_typed_and_exact_retry_is_not_duplicated() {
    use super::super::{record_root_batch, withdraw_root_batch};
    use crate::driver::ledger::{CommandKind, LedgerError};
    use crate::error::StoreRefusal;
    use crate::runtime::result_mail::{prepare_with_command, select};
    for live in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let rows = Rows::default();
        let mut session = SessionRecord::new(dir.path().to_owned());
        let item = QueueItem::new("continue");
        session.pending_queue.push(item.clone());
        rows.records.save(&mut session).unwrap();
        session = rows.records.load(&session.id).unwrap();
        let mut tree = result_tree(&session);
        rows.save_tree(&mut tree).unwrap();
        let runtime = live.then(|| {
            let runtime = crate::topology_runtime::insert_root_with(
                session.id.clone(),
                tree,
                rows.trees.clone(),
                None,
            )
            .block()
            .unwrap();
            // The capacity park ends at once, as on a host shutting down.
            let host = crate::agent_tree_tools::HostShutdown::new();
            host.signal();
            runtime.driver().bind_host_shutdown(host);
            runtime
        });
        let read_tree = || {
            runtime.as_ref().map_or_else(
                || rows.tree(&session.id).unwrap().unwrap(),
                |runtime| runtime.read(Clone::clone).unwrap(),
            )
        };
        let (command, batch) = select(&rows, &session, &item.text).unwrap();
        let batch = batch.unwrap();
        let id = format!("plak.queue.{}.{}", item.id, item.revision);
        let prepare = || {
            prepare_with_command(
                &rows,
                &rows.records,
                &session,
                &item,
                &command,
                Some(batch.clone()),
                &id,
                CommandKind::RootStart,
            )
        };
        rows.trees.full.store(true, Ordering::SeqCst);
        let error = prepare().await.unwrap_err();
        assert_eq!(error.refusal(), Some(StoreRefusal::StorageFull));
        let failure = crate::runtime::DriveTurnError::from(error)
            .refused_failure()
            .unwrap();
        assert_eq!(failure.store_refusal(), Some(StoreRefusal::StorageFull));
        assert_eq!(rows.records.load(&session.id).unwrap(), session);
        assert!(read_tree().command(&id).is_none());
        assert_eq!(read_tree().pending_mail("root").len(), 1);
        rows.trees.full.store(false, Ordering::SeqCst);
        // A crash after the tree write still retries the same immutable batch.
        record_root_batch(&rows, &id, &command, &batch)
            .await
            .unwrap();
        let first = read_tree().command(&id).unwrap().clone();
        let prepared = prepare().await.unwrap();
        let submission = rows
            .records
            .load_intents(&session.id)
            .unwrap()
            .submission(&id)
            .unwrap()
            .clone();
        assert_eq!(submission.canonical_command.as_bytes(), command.as_bytes());
        assert_eq!(
            prepared.queue_commands.pending_for(&item),
            Some(&submission)
        );
        assert_eq!(read_tree().command(&id), Some(&first));
        assert_eq!(
            read_tree()
                .commands()
                .values()
                .filter(|row| row.command_id == id)
                .count(),
            1
        );
        rows.trees.full.store(true, Ordering::SeqCst);
        let error = withdraw_root_batch(&rows, &id, &batch).await.unwrap_err();
        let error = RecordStoreError::from(LedgerError::from(error));
        assert_eq!(error.refusal(), Some(StoreRefusal::StorageFull));
        assert_eq!(read_tree().command(&id), Some(&first));
        rows.trees.full.store(false, Ordering::SeqCst);
        withdraw_root_batch(&rows, &id, &batch).await.unwrap();
        assert!(read_tree().command(&id).unwrap().withdrawn);
        assert_eq!(read_tree().pending_mail("root").len(), 1);
        if live {
            crate::topology_runtime::force_release(&session.id).await;
        }
    }
}

/// Another live host's custody of the tree stays typed through the root
/// batch written ahead of admission and through the projector's tree save:
/// both end with exit code 7 and the `tree_hosted_elsewhere` JSONL code,
/// not an internal failure, and nothing is written.
#[tokio::test]
async fn another_hosts_custody_stays_typed_through_the_ledger_and_the_projector() {
    use crate::agent_tree::{CustodyHost, HostInstance};
    use crate::driver::ledger::CommandKind;
    use crate::error::{CliFailure, ReadFailure, StoreRefusal};
    use crate::runtime::result_mail::{prepare_with_command, select};
    let dir = tempfile::tempdir().unwrap();
    let rows = Rows::default();
    let mut session = SessionRecord::new(dir.path().to_owned());
    let item = QueueItem::new("continue");
    session.pending_queue.push(item.clone());
    rows.records.save(&mut session).unwrap();
    session = rows.records.load(&session.id).unwrap();
    let mut tree = result_tree(&session);
    rows.save_tree(&mut tree).unwrap();
    let ours = rows
        .trees
        .custody_port()
        .custody(&session.id)
        .unwrap()
        .unwrap();
    let other = CustodyHost {
        host_id: "local".to_owned(),
        instance: HostInstance::parse("9-0000000000000009").unwrap(),
        binary: Some("0123456789ab".to_owned()),
        host_name: "another-machine".to_owned(),
    };
    rows.trees
        .custody_port()
        .take_custody(
            &session.id,
            &other,
            Some(&ProductAgentTree::holder_gone_for_test(CustodyClaim {
                tree_id: session.id.clone(),
                host: ours.holding.host.clone(),
                fence: ours.fence,
            })),
        )
        .unwrap();
    let revision = rows.tree(&session.id).unwrap().unwrap().revision;
    let assert_hosted_elsewhere = |failure: CliFailure| {
        assert_eq!(failure.exit_code(), 7, "{}", failure.message());
        assert_eq!(failure.code(), Some("tree_hosted_elsewhere"));
        assert!(
            failure
                .message()
                .starts_with("tree_hosted_elsewhere (0123456789ab): agent tree"),
            "{}",
            failure.message()
        );
        let line: serde_json::Value =
            serde_json::from_str(&plakous_host_protocol::envelope::coded_diagnostic_line(
                None,
                failure.code(),
                failure.message(),
            ))
            .unwrap();
        assert_eq!(line["code"], "tree_hosted_elsewhere");
    };

    let (command, batch) = select(&rows, &session, &item.text).unwrap();
    assert!(batch.is_some(), "the child's result rides the prompt");
    let id = format!("plak.queue.{}.{}", item.id, item.revision);
    let error = prepare_with_command(
        &rows,
        &rows.records,
        &session,
        &item,
        &command,
        batch,
        &id,
        CommandKind::RootStart,
    )
    .await
    .unwrap_err();
    assert_eq!(error.refusal(), Some(StoreRefusal::HostedElsewhere));
    assert_hosted_elsewhere(
        crate::runtime::DriveTurnError::from(error)
            .refused_failure()
            .unwrap(),
    );

    let mut tree = rows.tree(&session.id).unwrap().unwrap();
    let refused = rows.save_tree(&mut tree).unwrap_err();
    assert_hosted_elsewhere(CliFailure::from(ReadFailure::from(refused)));

    assert_eq!(rows.tree(&session.id).unwrap().unwrap().revision, revision);
    assert!(
        rows.tree(&session.id)
            .unwrap()
            .unwrap()
            .command(&id)
            .is_none()
    );
    assert_eq!(rows.records.load(&session.id).unwrap(), session);
}
