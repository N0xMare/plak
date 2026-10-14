//! Where a product session record is persisted.
//!
//! The record itself is `plakous_hosted::sessions`, re-exported here so a
//! consumer's `crate::store::sessions::…` path does not change twice.
//! Path-based helpers below are fixture access only; product commands use the
//! composition root's `SessionRecordStore`.

use std::fs;
use std::path::{Path, PathBuf};

pub use plakous_hosted::sessions::*;

#[cfg(any(test, feature = "test-support"))]
#[cfg(any(test, feature = "test-support"))]
use crate::home::paths::PlakPaths;

#[cfg(any(test, feature = "test-support"))]
pub fn save_session(paths: &PlakPaths, session: &mut SessionRecord) -> Result<(), String> {
    with_product_store(paths, |store| store.commit(session))
}

#[cfg(any(test, feature = "test-support"))]
pub fn load_session(paths: &PlakPaths, id: &str) -> Result<SessionRecord, String> {
    canonical_product_session_id(id)?;
    with_product_store(paths, |store| store.load(id))
}

#[cfg(any(test, feature = "test-support"))]
pub fn list_sessions_for_cwd(paths: &PlakPaths, cwd: &Path) -> Result<Vec<SessionRecord>, String> {
    with_product_store(paths, |store| store.list_for_workspace(cwd))
}

#[cfg(any(test, feature = "test-support"))]
fn with_product_store<T>(
    paths: &PlakPaths,
    operation: impl FnOnce(&mut crate::store::ProductStore) -> Result<T, String>,
) -> Result<T, String> {
    let mut store = crate::store::ProductStore::open(paths).map_err(|error| error.to_string())?;
    operation(&mut store)
}

#[must_use]
pub fn workspace_identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use std::ffi::OsStr;

    use tempfile::tempdir;

    use super::*;
    use crate::store::ProductStore;
    use plakous::sessions::SessionLocator;

    // Copied rather than shared: the product half's tests need the same three
    // fixtures the record's own tests do, and a `#[cfg(test)]` item cannot
    // cross a crate boundary.
    fn locator(checkpoint: u8) -> SessionLocator {
        format!(
            "00000000-0000-0000-0000-000000000001/\
             00000000-0000-0000-0000-000000000002/\
             00000000-0000-0000-0000-{checkpoint:012}"
        )
        .parse()
        .unwrap()
    }

    fn bound_session(workspace_root: impl Into<PathBuf>) -> SessionRecord {
        let mut session = SessionRecord::new(workspace_root.into());
        session.bind_plakous_locator(locator(3)).unwrap();
        session
    }

    #[test]
    fn session_round_trip() {
        let dir = tempdir().unwrap();
        let paths =
            PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
        let mut session = bound_session("/work/proj");
        session.account = Some("grok".into());
        session.model = Some("grok-4.5".into());
        session.record_notice(TranscriptRow::warning(NoticeScope::Tree, "hello"));
        session.record_prompt("hello".into());
        session.pending_queue.push(QueueItem::new("follow up"));
        let queued_id = session.pending_queue[0].id.clone();
        save_session(&paths, &mut session).unwrap();
        let loaded = load_session(&paths, &session.id).unwrap();
        assert_eq!(loaded.account.as_deref(), Some("grok"));
        assert_eq!(loaded.plakous_locator, Some(locator(3)));
        assert_eq!(loaded.transcript.len(), 1);
        assert_eq!(loaded.prompt_history, ["hello"]);
        assert_eq!(loaded.pending_queue[0].id, queued_id);
        assert_eq!(loaded.pending_queue[0].revision, 1);
        assert_eq!(loaded.pending_queue[0].text, "follow up");
        assert_eq!(
            loaded.pending_queue[0].submitted_at_ms,
            session.pending_queue[0].submitted_at_ms
        );
        assert_eq!(loaded.queue_drain_state, QueueDrainState::RestoredPaused);
    }

    #[test]
    fn queued_prompt_submit_time_is_durable_and_first_write_wins() {
        let dir = tempdir().unwrap();
        let paths = PlakPaths::explicit(dir.path().join("product"));
        let mut session = bound_session("/work/submit-time");
        let mut item = QueueItem::new("later");
        item.submitted_at_ms = 1_790_000_000_000;
        session.pending_queue.push(item);
        save_session(&paths, &mut session).unwrap();
        // An edit is a new revision of the same item; its submit time is the
        // operator's original one, and the store never rewrites it.
        session.pending_queue[0].revision = 2;
        "later, edited".clone_into(&mut session.pending_queue[0].text);
        session.pending_queue[0].submitted_at_ms = 5;
        save_session(&paths, &mut session).unwrap();
        let loaded = load_session(&paths, &session.id).unwrap();
        assert_eq!(loaded.pending_queue[0].text, "later, edited");
        assert_eq!(loaded.pending_queue[0].submitted_at_ms, 1_790_000_000_000);
    }

    #[test]
    fn aggregate_queue_byte_bound_is_enforced_by_record_and_store() {
        let mut session = bound_session("/work/queue-aggregate-bounds");
        let item_bytes = MAX_PENDING_QUEUE_BYTES / 17 + 1;
        assert!(item_bytes < MAX_PROMPT_BYTES);
        session.pending_queue = (0..17)
            .map(|_| QueueItem::new("x".repeat(item_bytes)))
            .collect();
        assert_eq!(
            session.validate_for_store().unwrap_err(),
            "session has invalid pending queue state"
        );
        let dir = tempdir().unwrap();
        let paths =
            PlakPaths::resolve(Some(dir.path().as_os_str()), Some(OsStr::new("/home/u"))).unwrap();
        let mut persisted = bound_session("/work/mutation-aggregate");
        save_session(&paths, &mut persisted).unwrap();
        persisted.pending_queue = session.pending_queue;
        assert_eq!(
            save_session(&paths, &mut persisted).unwrap_err(),
            "session has invalid pending queue state"
        );
        assert_eq!(
            load_session(&paths, &persisted.id).unwrap().pending_queue,
            [] as [QueueItem; 0]
        );
    }

    #[test]
    fn scheduled_compaction_reason_is_durable_and_exact() {
        let mut session = bound_session("/work/compact-reason");
        let request = projector::WorkflowRequest {
            operation_id: "plak:compact:stable".into(),
            source_locator: session.plakous_locator.unwrap(),
            kind: SessionWorkflowKind::Compact(ProductCompactReason::Scheduled),
        };
        let dir = tempdir().unwrap();
        let paths = PlakPaths::explicit(dir.path().join("product"));
        let mut store = ProductStore::open(&paths).unwrap();
        store.commit(&mut session).unwrap();
        store
            .submit_workflow_request(&mut session, &request)
            .unwrap();
        drop(store);
        let mut reopened = ProductStore::open(&paths).unwrap();
        assert_eq!(
            reopened
                .load_intents(&session.id)
                .unwrap()
                .workflow_requests,
            vec![request]
        );
    }

    #[test]
    fn capacity_recovery_compaction_reason_is_durable_and_exact() {
        let mut session = bound_session("/work/compact-reason");
        let request = projector::WorkflowRequest {
            operation_id: "plak:compact:stable".into(),
            source_locator: session.plakous_locator.unwrap(),
            kind: SessionWorkflowKind::Compact(ProductCompactReason::CapacityRecovery),
        };
        let dir = tempdir().unwrap();
        let paths = PlakPaths::explicit(dir.path().join("product"));
        let mut store = ProductStore::open(&paths).unwrap();
        store.commit(&mut session).unwrap();
        store
            .submit_workflow_request(&mut session, &request)
            .unwrap();
        drop(store);
        let mut reopened = ProductStore::open(&paths).unwrap();
        assert_eq!(
            reopened
                .load_intents(&session.id)
                .unwrap()
                .workflow_requests,
            vec![request]
        );
    }

    #[test]
    fn duplicate_text_items_keep_distinct_identity_across_store_round_trip() {
        let mut session = bound_session("/work/duplicates");
        session.pending_queue = vec![QueueItem::new("repeat"), QueueItem::new("repeat")];
        session.pending_queue[1].revision = 3;
        let expected = session.pending_queue.clone();

        let dir = tempdir().unwrap();
        let paths = PlakPaths::explicit(dir.path().join("product"));
        save_session(&paths, &mut session).unwrap();
        let loaded = load_session(&paths, &session.id).unwrap();

        assert_eq!(loaded.pending_queue, expected);
        assert_ne!(loaded.pending_queue[0].id, loaded.pending_queue[1].id);
        assert_eq!(loaded.pending_queue[1].revision, 3);
        assert_eq!(loaded.queue_drain_state, QueueDrainState::RestoredPaused);
    }
}
