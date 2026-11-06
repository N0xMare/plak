//! A child turn's end is folded after the release that ended it, and only
//! through that turn's own terminal (spec 03, child commands).

use super::*;
use crate::topology_runtime::{LiveChildHandle, ProductTopologyRuntime};
use plakous::agent::TurnControl;
use plakous::model::ModelErrorKind;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenedSession, SessionBackend,
    SessionOperationId,
};
use plakous_offline_crash_harness::protocol::CrashBoundary;

fn runtime(root: &Path) -> Arc<ProductTopologyRuntime> {
    Arc::new(ProductTopologyRuntime::new(
        crate::topology_runtime::test_support::bootstrap("child-turn-end", root.to_path_buf()),
        crate::topology_runtime::test_support::memory_store(),
        Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ))
}

/// A child session over a model whose first provider dispatch waits for
/// `release` to exist and then fails.
struct GatedSession {
    sessions: Arc<AgentSessions>,
    session: OpenedSession,
    evidence: PathBuf,
    release: PathBuf,
}

async fn gated_session(root: &Path, label: &str) -> GatedSession {
    let (evidence, release) = (root.join("evidence.jsonl"), root.join("release"));
    let controller =
        plakous_offline_crash_harness::BoundaryController::quiet_gated_from_evidence_path(
            CrashBoundary::ProviderDispatchOne,
            &evidence,
            &release,
        )
        .unwrap();
    let binding = plakous_offline_crash_harness::terminal_failure_runtime_binding(
        root,
        controller,
        ModelErrorKind::Protocol,
    )
    .unwrap();
    let backend = SessionBackend::sqlite(root.join("runtime.sqlite3")).unwrap();
    let sessions = Arc::new(AgentSessions::new(backend).unwrap());
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new(format!("{label}:create")).unwrap(),
            binding,
        ))
        .await
        .unwrap();
    let CreateSessionOutcome::Created { session, .. } = created else {
        unreachable!("a fresh session is created")
    };
    GatedSession {
        sessions,
        session,
        evidence,
        release,
    }
}

/// Waits until the model announced its first provider dispatch.
async fn dispatched(evidence: &Path) {
    // A liveness bound only: generous, so load never trips it.
    tokio::time::timeout(Duration::from_secs(20), async {
        while !plakous_offline_crash_harness::evidence::read_evidence(evidence).is_ok_and(|rows| {
            rows.iter()
                .any(|row| row.boundary == CrashBoundary::ProviderDispatchOne)
        }) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

/// Spawns a child of the root bound to `turn` on the session at `locator`.
async fn bound_child(
    runtime: &ProductTopologyRuntime,
    turn: TurnId,
    locator: SessionLocator,
) -> String {
    runtime
        .mutate_then_emit(false, move |tree| {
            let child = tree
                .spawn_agent(
                    "root",
                    &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .map_err(|error| error.message)?
                .agent_id;
            tree.bind_attempt_turn(&child, turn.to_string())
                .map_err(|error| error.message)?;
            tree.set_active_turn(&child, true)
                .map_err(|error| error.message)?;
            tree.bind_session_locator(&child, locator.to_string())
                .map_err(|error| error.message)?;
            Ok(child)
        })
        .await
        .unwrap()
}

/// Waits until `child`'s turn waiter left and its live slot cleared.
async fn waiter_done(runtime: &ProductTopologyRuntime, child: &str) {
    // A liveness bound only: generous, so load never trips it.
    tokio::time::timeout(Duration::from_secs(30), async {
        while runtime
            .read(|tree| tree.has_turn_waiter(child))
            .unwrap_or(true)
            || !runtime.live.lock().await.is_empty()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

/// A live child whose retirement holds its finished release, before the hub
/// publishes the release page, until `publish` is signalled.
struct HeldPublication {
    inner: Box<dyn LiveChildHandle>,
    released: Arc<tokio::sync::Notify>,
    publish: Arc<tokio::sync::Notify>,
}

impl LiveChildHandle for HeldPublication {
    fn host_observation(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.inner.host_observation()
    }
    fn agent(&self) -> Option<plakous::agent::Agent> {
        self.inner.agent()
    }
    fn reachability_probe(&self) -> Option<plakous::model::ReachabilityProbe> {
        self.inner.reachability_probe()
    }
    fn turn_id(&self) -> TurnId {
        self.inner.turn_id()
    }
    fn turn_control(&self) -> TurnControl {
        self.inner.turn_control()
    }
    fn feed(
        &self,
        cursor: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<crate::agent_tree::TreeFeed>, String>> + Send>>
    {
        self.inner.feed(cursor)
    }
    fn follow_up(
        &self,
        command_id: CommandId,
        body: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        self.inner.follow_up(command_id, body)
    }
    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        self.inner.follow_up_barrier()
    }
    fn take_follow_up(&mut self) -> Option<TurnResultHandle> {
        self.inner.take_follow_up()
    }
    fn restart(
        &mut self,
        driver: TreeDriver,
        command_id: CommandId,
        prompt: String,
        delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
        self.inner.restart(driver, command_id, prompt, delivery)
    }
    fn release(
        self: Box<Self>,
        driver: TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        let Self {
            inner,
            released,
            publish,
        } = *self;
        Box::pin(async move {
            let release = inner.release(driver).await;
            released.notify_one();
            publish.notified().await;
            release
        })
    }
    fn cancel_and_release(
        self: Box<Self>,
        driver: TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        let Self {
            inner,
            released,
            publish,
        } = *self;
        Box::pin(async move {
            let release = inner.cancel_and_release(driver, dispatched).await;
            released.notify_one();
            publish.notified().await;
            release
        })
    }
}

/// A turn the host's retirement cancelled ends while that retirement still
/// publishes its release page: the turn waiter folds the turn's end only
/// once that page is in, so the release's and the waiter's tree writes have
/// one order whichever ran first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_turns_end_folds_after_its_release_page() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let GatedSession {
        sessions,
        session,
        evidence,
        release: _,
    } = gated_session(root.path(), "release-order").await;
    let submission = session
        .agent()
        .start(CommandId::new("release-order:start").unwrap(), "held turn")
        .await
        .unwrap();
    let handle = submission.result().unwrap().clone();
    let turn = handle.turn_id();
    let control = session.agent().turn_control(turn).unwrap();
    dispatched(&evidence).await;
    let locator = session.locator();
    let child = bound_child(&runtime, turn, locator).await;
    let feed = ChildFeedSource {
        sessions: Arc::clone(&sessions),
        session: locator.session_id(),
    };
    let live_child = super::super::live::ChildActorHandle::new(
        &runtime.driver().release_owners(),
        AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions,
            opened: Some(session),
            recovered_turn: None,
            locator,
            topology_owner: None,
        },
        turn,
        control,
    );
    let watcher = live_child.watcher();
    let (released, publish) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    assert!(
        runtime
            .try_occupy_live(
                child.clone(),
                Box::new(HeldPublication {
                    inner: Box::new(live_child),
                    released: released.clone(),
                    publish: publish.clone(),
                }),
            )
            .await
            .is_ok()
    );
    watcher.watch(
        TurnWaiterRegistration::register(runtime.driver(), &child)
            .await
            .unwrap(),
        runtime.live.clone(),
        None,
        Some(feed),
        handle.clone(),
    );
    let key = locator.session_id().to_string();
    let cursor = || runtime.read(|tree| tree.feed_cursor(&key)).unwrap();
    let before = cursor();
    let retired = runtime
        .begin_cancel_live_many(std::slice::from_ref(&child))
        .await;
    released.notified().await;
    let ended = handle.clone().result().await.unwrap();
    assert!(
        matches!(ended.outcome(), TurnResultOutcome::Cancelled(_)),
        "{:?}",
        ended.outcome()
    );
    // The waiter has the cancelled terminal; its fold must wait for the page.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        cursor(),
        before,
        "the waiter folded the turn's end before the release page"
    );
    publish.notify_one();
    let _ = retired.await;
    waiter_done(&runtime, &child).await;
    assert!(cursor() > before, "the turn's end was never folded");
}

/// A turn's end folds the child's feed only through that turn's terminal:
/// records the child committed after it (here, its session's release) are
/// left to the tree's next record of the child, so how far the child got
/// before the waiter read never moves the fold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_end_fold_stops_at_its_terminal_when_the_child_committed_past_it() {
    use plakous::sessions::feed::FeedFact;
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let GatedSession {
        sessions,
        session,
        evidence: _,
        release,
    } = gated_session(root.path(), "terminal-bound").await;
    std::fs::write(&release, "release").unwrap();
    let submission = session
        .agent()
        .start(
            CommandId::new("terminal-bound:start").unwrap(),
            "failing turn",
        )
        .await
        .unwrap();
    let handle = submission.result().unwrap().clone();
    let turn = handle.turn_id();
    let ended = handle.clone().result().await.unwrap();
    assert!(
        matches!(ended.outcome(), TurnResultOutcome::Failed(_)),
        "{:?}",
        ended.outcome()
    );
    let locator = session.locator();
    // The child commits past its terminal before the waiter reads.
    session.agent().shutdown().await.unwrap();
    drop(session);
    let child = bound_child(&runtime, turn, locator).await;
    let records = crate::agent::read_tree_feed(&sessions, locator.session_id(), 0)
        .await
        .unwrap()
        .records;
    let terminal = records
        .iter()
        .find(|record| {
            record.facts().iter().any(
                |fact| matches!(fact, FeedFact::TurnTerminal { turn_id, .. } if *turn_id == turn),
            )
        })
        .map(|record| record.header().seq.get())
        .unwrap();
    let tail = records
        .last()
        .map(|record| record.header().seq.get())
        .unwrap();
    assert!(
        tail > terminal,
        "the child committed nothing past its terminal"
    );
    let feed = ChildFeedSource {
        sessions: Arc::clone(&sessions),
        session: locator.session_id(),
    };
    let registration = TurnWaiterRegistration::register(runtime.driver(), &child)
        .await
        .unwrap();
    // A liveness bound only: generous, so load never trips it.
    tokio::time::timeout(
        Duration::from_secs(30),
        turn_watch(registration, runtime.live.clone(), None, Some(feed), handle),
    )
    .await
    .unwrap();
    let key = locator.session_id().to_string();
    assert_eq!(
        runtime.read(|tree| tree.feed_cursor(&key)).unwrap(),
        terminal,
        "the turn's end folded past its terminal"
    );
}
