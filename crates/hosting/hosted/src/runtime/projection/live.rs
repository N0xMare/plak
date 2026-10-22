//! The live projector tick (spec 02 §2.6): one background task per attached
//! session applies the feed to the views after each acknowledged commit. The
//! host loop never waits on it while streaming; terminal publication and
//! release grant it a bounded final pass. The tree projector runs
//! from the host loop itself. Projection debt is safe:
//! every write is CAS'd on the cursor and the next open catches up.

use std::sync::Arc;
use std::time::Duration;

use plakous::sessions::{AgentSessions, SessionLocator};
use tokio::sync::{Notify, watch};

use crate::agent::ProductStateRoots;
use crate::session_store::SessionRecordStore;
use crate::sessions::projector::{RowKey, SessionViews};
use crate::sessions::{ToolItem, TranscriptRow};

use super::super::topology_state::HostRows;

/// What the task projects: the product session, the runtime head it follows
/// and the tree the same feed drives.
#[derive(Clone)]
pub(crate) struct LiveTarget {
    pub(crate) sessions: Arc<AgentSessions>,
    pub(crate) store: Arc<dyn SessionRecordStore>,
    pub(crate) roots: ProductStateRoots,
    pub(crate) product_id: String,
    pub(crate) followed: SessionLocator,
    pub(crate) tree_id: String,
    pub(crate) emit_settled: Arc<dyn Fn(TranscriptRow) + Send + Sync>,
}

impl LiveTarget {
    fn same_session(&self, other: &Self) -> bool {
        self.product_id == other.product_id
            && self.followed.session_id() == other.followed.session_id()
    }

    /// One product projector pass to the feed's tail.
    pub(crate) async fn pass(&self) -> Result<(), String> {
        self.views(None).await.map(|_| ())
    }

    pub(in crate::runtime) async fn views(
        &self,
        cached: Option<SessionViews>,
    ) -> Result<super::ProjectedViews, String> {
        let pass = super::project_to_tail_cached(
            &self.sessions,
            &self.store,
            &self.product_id,
            self.followed,
            cached,
        );
        #[cfg(test)]
        let pass = super::probe::CALLER.scope(super::probe::Caller::Live, pass);
        pass.await.map_err(String::from)
    }

    /// The tree projector over the same feed. The host runs it from its
    /// loop, not on the task: tree saves stay ordered with the tree's own
    /// writes.
    pub(crate) async fn tree_pass(&self) -> Result<(), String> {
        super::project_tree(
            &self.sessions,
            &HostRows::new(&self.roots, &*self.store),
            &self.tree_id,
            self.followed.session_id(),
        )
        .await?;
        if let Some(runtime) = crate::topology_runtime::lookup(&self.tree_id) {
            runtime
                .driver()
                .reclaim_dormant_checkouts()
                .await
                .map_err(|error| error.message)?;
            runtime
                .driver()
                .collect_dormant_children(&self.sessions)
                .await
                .map_err(|error| error.message)?;
        }
        Ok(())
    }
}

/// A pass the task finished: the request generation it covered and its
/// refusal, if any.
#[derive(Clone, Default)]
struct Applied {
    generation: u64,
    error: Option<String>,
}

#[derive(Clone)]
struct Request {
    generation: u64,
    target: LiveTarget,
    finish: bool,
}

/// The host loop's handle on its session's projector task.
pub(crate) struct LiveProjector {
    requests: watch::Sender<Option<Request>>,
    wake: Arc<Notify>,
    applied: watch::Receiver<Applied>,
    generation: u64,
    reported: u64,
    task: tokio::task::JoinHandle<Option<SessionViews>>,
}

impl LiveProjector {
    /// Spawns the task on the current runtime. Call it inside the host's
    /// executor.
    pub(crate) fn spawn() -> Self {
        let (requests, receiver) = watch::channel(None);
        let (report, applied) = watch::channel(Applied::default());
        let wake = Arc::new(Notify::new());
        let task = tokio::spawn(run(receiver, Arc::clone(&wake), report));
        Self {
            requests,
            wake,
            applied,
            generation: 0,
            reported: 0,
            task,
        }
    }

    /// Asks for a pass over `target` and returns at once. Requests coalesce:
    /// the task runs one pass for every burst, always over the latest target.
    pub(crate) fn tick(&mut self, target: LiveTarget) {
        self.generation = self.generation.saturating_add(1);
        let _ = self.requests.send(Some(Request {
            generation: self.generation,
            target,
            finish: false,
        }));
        self.wake.notify_one();
    }

    /// The refusal of a pass finished since the last call, once.
    pub(crate) fn take_refusal(&mut self) -> Option<String> {
        let applied = self.applied.borrow();
        if applied.generation <= self.reported {
            return None;
        }
        self.reported = applied.generation;
        applied.error.clone()
    }

    /// Requests a final pass over `target` and waits for it, at most `bound`
    /// when one is given. Returns its owned views, or the debt left behind.
    /// The task stops either way.
    /// Its workflow bookkeeping may lag same-cursor joins performed by the
    /// store; returned state is the projector's view, not a fresh store read.
    pub(crate) async fn finish(
        mut self,
        target: LiveTarget,
        bound: Option<Duration>,
    ) -> Result<SessionViews, String> {
        self.generation = self.generation.saturating_add(1);
        let _ = self.requests.send(Some(Request {
            generation: self.generation,
            target,
            finish: true,
        }));
        self.wake.notify_one();
        let task = &mut self.task;
        let finished = match bound {
            Some(bound) => tokio::time::timeout(bound, task).await,
            None => Ok(task.await),
        };
        match finished {
            Ok(Ok(Some(views))) => Ok(views),
            Ok(Ok(None)) => Err(self
                .applied
                .borrow()
                .error
                .clone()
                .unwrap_or_else(|| "the projector task stopped".to_owned())),
            Ok(Err(_)) => Err("the projector task stopped".to_owned()),
            Err(_) => Err("the final projector pass exceeded its bound".to_owned()),
        }
    }
}

impl Drop for LiveProjector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Wakes the task on a request or on a commit of the followed session. A
/// forwarder task turns the feed subscription into wakes; it is replaced
/// when the followed session changes.
async fn run(
    mut requests: watch::Receiver<Option<Request>>,
    wake: Arc<Notify>,
    report: watch::Sender<Applied>,
) -> Option<SessionViews> {
    let mut current: Option<Request> = None;
    let mut cached = None;
    let mut forwarder: Option<tokio::task::JoinHandle<()>> = None;
    let mut shown = SettledRows::default();
    loop {
        wake.notified().await;
        if requests.has_changed().is_err() {
            break;
        }
        if requests.has_changed().unwrap_or(false) {
            let Some(next) = requests.borrow_and_update().clone() else {
                continue;
            };
            if current
                .as_ref()
                .is_none_or(|current| !current.target.same_session(&next.target))
            {
                cached = None;
            }
            if forwarder
                .as_ref()
                .is_none_or(tokio::task::JoinHandle::is_finished)
                || current
                    .as_ref()
                    .is_none_or(|current| !current.target.same_session(&next.target))
            {
                shown = SettledRows::default();
                if let Some(old) = forwarder.take() {
                    old.abort();
                }
                if let Ok(mut feed) = next
                    .target
                    .sessions
                    .subscribe_feed(next.target.followed.session_id())
                    .await
                {
                    let wake = Arc::clone(&wake);
                    // A lost subscription ends the forwarder; the next request
                    // resubscribes.
                    forwarder = Some(tokio::spawn(async move {
                        while feed.changed().await.is_ok() {
                            wake.notify_one();
                        }
                    }));
                }
            }
            current = Some(next);
        }
        let Some(request) = &current else {
            continue;
        };
        let error = match request.target.views(cached.take()).await {
            Ok(super::ProjectedViews { views, changed }) => {
                let current_target = requests
                    .borrow()
                    .as_ref()
                    .is_some_and(|latest| latest.target.same_session(&request.target));
                if current_target {
                    emit_settled_changes(
                        &views,
                        changed,
                        &mut shown,
                        &*request.target.emit_settled,
                    );
                }
                cached = Some(views);
                None
            }
            Err(error) => Some(error),
        };
        report.send_replace(Applied {
            generation: request.generation,
            error,
        });
        if request.finish {
            break;
        }
    }
    if let Some(forwarder) = forwarder {
        forwarder.abort();
    }
    cached
}

#[derive(Default)]
struct SettledRows {
    keys: std::collections::HashMap<RowKey, String>,
    calls: std::collections::HashMap<String, ToolItem>,
}

fn emit_settled_changes(
    views: &SessionViews,
    changed: Option<std::collections::BTreeSet<RowKey>>,
    shown: &mut SettledRows,
    emit: &dyn Fn(TranscriptRow),
) {
    let changed = changed.unwrap_or_else(|| {
        views
            .rows
            .keys()
            .chain(shown.keys.keys())
            .copied()
            .collect()
    });
    let mut removed: std::collections::HashSet<_> = changed
        .iter()
        .filter_map(|key| shown.keys.remove(key))
        .collect();
    // A retention rebuild can move a call to another row key. Compare call
    // identities before retiring deleted keys, as the full-row scan did.
    for key in changed {
        let Some(tool) = views
            .rows
            .get(&key)
            .and_then(TranscriptRow::as_tool)
            .filter(|tool| tool.state != "running")
        else {
            continue;
        };
        if shown.calls.get(&tool.call_id) != Some(tool) {
            emit(TranscriptRow::Tool(tool.clone()));
            shown.calls.insert(tool.call_id.clone(), tool.clone());
        }
        removed.remove(&tool.call_id);
        shown.keys.insert(key, tool.call_id.clone());
    }
    for call in removed {
        shown.calls.remove(&call);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;
    use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
    use crate::session_store::MemoryStore;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct RefuseCommit(Arc<AtomicBool>);

    impl StoreFaults for RefuseCommit {
        fn before(
            &self,
            call: StoreCall,
            _: Option<&crate::sessions::SessionRecord>,
        ) -> Result<(), String> {
            if call == StoreCall::CommitProjectionViews && self.0.load(Ordering::SeqCst) {
                return Err("injected projection commit failure".into());
            }
            Ok(())
        }
    }

    #[test]
    fn a_failed_live_page_drops_its_views_and_the_next_pass_reloads() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) =
            crate::runtime::test_support::fresh_injected_attached_host(root.path());
        let store = Arc::new(MemoryStore::new());
        config.store = store.clone();
        let id = config.session.id.clone();
        super::super::tests::run_prompts(config, &["recorded turn"]);
        let record = store.load(&id).unwrap();
        let (config, _) = crate::runtime::test_support::fresh_injected_attached_host(root.path());
        let refuse = Arc::new(AtomicBool::new(true));
        let target = LiveTarget {
            sessions: config.state_roots.runtime_sessions().unwrap(),
            store: Arc::new(FaultStore::new(
                store.clone(),
                RefuseCommit(Arc::clone(&refuse)),
            )),
            roots: config.state_roots,
            product_id: id.clone(),
            followed: record.plakous_locator.unwrap(),
            tree_id: id,
            emit_settled: Arc::new(|_| {}),
        };
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let records: Arc<dyn SessionRecordStore> = store.clone();
        let expected = executor
            .block_on(super::super::project_to_tail(
                &target.sessions,
                &records,
                &target.product_id,
                target.followed,
            ))
            .unwrap()
            .unwrap();
        store.delete_views(&target.product_id);
        executor
            .block_on(async {
                let mut live = LiveProjector::spawn();
                let reads = store.view_read_count();
                live.tick(target.clone());
                live.applied
                    .wait_for(|applied| applied.generation == 1)
                    .await
                    .unwrap();
                assert_eq!(
                    live.take_refusal().as_deref(),
                    Some("injected projection commit failure")
                );
                assert_eq!(store.view_read_count(), reads + 1);
                refuse.store(false, Ordering::SeqCst);
                let actual = live.finish(target.clone(), None).await.unwrap();
                assert_eq!(
                    store.view_read_count(),
                    reads + 2,
                    "the failed page cannot retain mutated views"
                );
                assert_eq!(actual, expected);
                let mut live = LiveProjector::spawn();
                live.tick(target.clone());
                live.applied
                    .wait_for(|applied| applied.generation == 1)
                    .await
                    .unwrap();
                let reads = store.view_read_count();
                let records = store.record_read_count();
                assert_eq!(live.finish(target, None).await.unwrap(), expected);
                assert_eq!(
                    store.view_read_count(),
                    reads,
                    "finish reuses the live views"
                );
                assert_eq!(
                    store.record_read_count(),
                    records,
                    "finish needs no full record"
                );
            })
            .unwrap();
    }

    #[test]
    fn a_facade_read_failure_on_page_two_reloads_the_last_committed_page() {
        use crate::sessions::projector::{ProductIntents, read_fault, run};
        use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) =
            crate::runtime::test_support::fresh_injected_attached_host(root.path());
        let store = Arc::new(MemoryStore::new());
        config.store = store.clone();
        let id = config.session.id.clone();
        super::super::tests::run_prompts(config, &["recorded turn"; 80]);
        let followed = store.load(&id).unwrap().plakous_locator.unwrap();
        let (config, _) = crate::runtime::test_support::fresh_injected_attached_host(root.path());
        let sessions = config.state_roots.runtime_sessions().unwrap();
        let records = super::super::tests::feed_records(&sessions, followed.session_id());
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        executor
            .block_on(async {
                let FeedPage::Records { records: first, .. } = sessions
                    .feed_page(FeedPageRequest::after(FeedCursor::start(
                        followed.session_id(),
                    )))
                    .await
                    .unwrap()
                else {
                    panic!("retained feed required")
                };
                assert!(records.len() > first.len(), "fixture must span pages");
                let mut intents: ProductIntents = store.load_intents(&id).unwrap();
                intents
                    .submissions
                    .extend(store.load_inherited_submissions(&id).unwrap());
                let prefix = run(&sessions, SessionViews::empty(), &intents, &first)
                    .await
                    .unwrap()
                    .views;
                let expected = run(&sessions, SessionViews::empty(), &intents, &records)
                    .await
                    .unwrap()
                    .views;
                store.delete_views(&id);
                read_fault::arm(followed.session_id(), prefix.cursor);
                let target = LiveTarget {
                    sessions,
                    store: store.clone(),
                    roots: config.state_roots,
                    product_id: id.clone(),
                    followed,
                    tree_id: id.clone(),
                    emit_settled: Arc::new(|_| {}),
                };
                let reads = store.view_read_count();
                let mut live = LiveProjector::spawn();
                live.tick(target.clone());
                live.applied
                    .wait_for(|applied| applied.generation == 1)
                    .await
                    .unwrap();
                assert!(
                    live.take_refusal()
                        .unwrap()
                        .starts_with("projector read failed:")
                );
                assert_eq!(store.view_read_count(), reads + 1);
                assert_eq!(store.load_views(&id).unwrap(), Some(prefix));
                let reads = store.view_read_count();
                assert_eq!(live.finish(target, None).await.unwrap(), expected);
                assert_eq!(
                    store.view_read_count(),
                    reads + 1,
                    "read error must discard live cached views"
                );
                assert_eq!(store.load_views(&id).unwrap(), Some(expected));
            })
            .unwrap();
    }

    #[test]
    fn changed_keys_emit_the_same_settled_rows_as_a_full_scan() {
        use crate::sessions::projector::{Lane, ViewWrites};
        use std::cell::RefCell;
        let tool = |call: &str, state: &str| {
            TranscriptRow::Tool(ToolItem {
                call_id: call.into(),
                name: "bash".into(),
                exposed_name: None,
                kind: crate::activity::tool_activity_kind("bash"),
                state: state.into(),
                subject: None,
                detail: None,
                started_at_ms: None,
                finished_at_ms: None,
                parent_call_id: None,
                cell_id: None,
                wait: None,
            })
        };
        let a = RowKey::new(1, Lane::Runtime, 0);
        let b = RowKey::new(2, Lane::Runtime, 0);
        let c = RowKey::new(3, Lane::Runtime, 0);
        let mut views = SessionViews::empty();
        let mut shown = SettledRows::default();
        let mut scanned = std::collections::HashMap::new();
        for changes in [
            vec![(a, Some(tool("a", "running")))],
            vec![(b, Some(tool("b", "succeeded")))],
            vec![(a, Some(tool("a", "succeeded")))],
            vec![(a, None), (c, Some(tool("a", "succeeded")))],
            vec![(b, Some(tool("b", "failed")))],
            vec![(c, None)],
            vec![(b, Some(tool("b", "running")))],
            vec![(b, Some(tool("b", "succeeded")))],
        ] {
            let before = views.clone();
            for (key, row) in changes {
                if let Some(row) = row {
                    views.rows.insert(key, row);
                } else {
                    views.rows.remove(&key);
                }
            }
            let writes = ViewWrites::between(&before, &views);
            let changed = writes
                .upserts
                .iter()
                .map(|(key, _)| *key)
                .chain(writes.deletes)
                .collect();
            let mut retained = std::collections::HashMap::new();
            let mut expected = Vec::new();
            for row in views.rows.values() {
                if let Some(tool) = row.as_tool().filter(|tool| tool.state != "running") {
                    if scanned.get(&tool.call_id) != Some(tool) {
                        expected.push(row.clone());
                    }
                    retained.insert(tool.call_id.clone(), tool.clone());
                }
            }
            scanned = retained;
            let emitted = RefCell::new(Vec::new());
            emit_settled_changes(&views, Some(changed), &mut shown, &|row| {
                emitted.borrow_mut().push(row);
            });
            assert_eq!(*emitted.borrow(), expected);
            emitted.borrow_mut().clear();
            emit_settled_changes(&views, None, &mut shown, &|row| {
                emitted.borrow_mut().push(row);
            });
            assert!(
                emitted.borrow().is_empty(),
                "reloading cannot duplicate settled rows"
            );
        }
    }
}
