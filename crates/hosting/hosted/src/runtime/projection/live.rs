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
use crate::sessions::projector::SessionViews;
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

    async fn views(&self, cached: Option<SessionViews>) -> Result<SessionViews, String> {
        let pass = super::project_to_tail_cached(
            &self.sessions,
            &self.store,
            &self.product_id,
            self.followed,
            cached,
        );
        #[cfg(test)]
        let pass = super::probe::CALLER.scope(super::probe::Caller::Live, pass);
        pass.await
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
}

/// The host loop's handle on its session's projector task.
pub(crate) struct LiveProjector {
    requests: watch::Sender<Option<Request>>,
    wake: Arc<Notify>,
    applied: watch::Receiver<Applied>,
    generation: u64,
    reported: u64,
    task: tokio::task::JoinHandle<()>,
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
    /// when one is given. Returns the debt left behind: the pass's refusal,
    /// or that it exceeded its bound. The task stops either way.
    pub(crate) async fn finish(
        mut self,
        target: LiveTarget,
        bound: Option<Duration>,
    ) -> Option<String> {
        self.tick(target);
        let wanted = self.generation;
        let mut applied = self.applied.clone();
        let done = applied.wait_for(|applied| applied.generation >= wanted);
        let finished = match bound {
            Some(bound) => tokio::time::timeout(bound, done).await,
            None => Ok(done.await),
        };
        self.task.abort();
        match finished {
            Ok(Ok(applied)) => applied.error.clone(),
            Ok(Err(_)) => Some("the projector task stopped".to_owned()),
            Err(_) => Some("the final projector pass exceeded its bound".to_owned()),
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
) {
    let mut current: Option<Request> = None;
    let mut cached = None;
    let mut forwarder: Option<tokio::task::JoinHandle<()>> = None;
    let mut shown: std::collections::HashMap<String, ToolItem> =
        std::collections::HashMap::default();
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
                shown.clear();
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
        let error = match request.target.views(cached.clone()).await {
            Ok(views) => {
                let current_target = requests
                    .borrow()
                    .as_ref()
                    .is_some_and(|latest| latest.target.same_session(&request.target));
                let mut retained = std::collections::HashMap::new();
                for row in views.rows.values().filter(|_| current_target) {
                    let TranscriptRow::Tool(tool) = row else {
                        continue;
                    };
                    if tool.state == "running" {
                        continue;
                    }
                    if shown.get(&tool.call_id) != Some(tool) {
                        (request.target.emit_settled)(row.clone());
                    }
                    retained.insert(tool.call_id.clone(), tool.clone());
                }
                if current_target {
                    shown = retained;
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
    }
    if let Some(forwarder) = forwarder {
        forwarder.abort();
    }
}
