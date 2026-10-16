//! Recovery over real child sessions: a real runtime database, a real
//! unknown-outcome turn, the process hub, and the product's own reopen
//! (`ParentSessionPort` over `reopen_spawned_child`). The child binding is the
//! offline fixture shaped like a product coding session, so the product's
//! projection gate admits its reopen.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use plakous::agent::{CommandId, TurnResultOutcome};
use plakous::model::ModelErrorKind;
use plakous::sessions::{OpenSessionOutcome, OpenSessionRequest, OpenedSession, SessionLocator};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{
    BoundaryController, projected_terminal_failure_runtime_binding,
};
use serde_json::json;

use super::*;
use crate::agent::children::spawn_topology_turn_waiter;
use crate::agent::{ApiTopologyParent, NATIVE_RUN_MAX_MODEL_ROUNDS, ProductStateRoots};
use crate::agent_tree::{
    MemoryTreeStore, PersistedTree, ProductAgentTree, RECOVERED_WITHOUT_TURN, Revision,
    TURN_ABANDONED, TreeStore, TreeStoreError,
};
use crate::agent_tree_protocol::{
    AdoptCandidateRequest, AttemptStatus, MessagePurpose, NodeStatus, SendAgentMessageRequest,
    SpawnAgentRequest, Tier, WriterMode,
};
use crate::binding::{CompositionSelection, ResolvedProtocol};

/// A tree store whose publishes can be refused on demand.
#[derive(Default)]
struct SwitchableStore {
    inner: MemoryTreeStore,
    refuse: AtomicBool,
    refuse_sealed: AtomicBool,
    refuse_seal_failed: AtomicBool,
}

impl TreeStore for SwitchableStore {
    fn exists(&self, tree_id: &str) -> Result<bool, TreeStoreError> {
        self.inner.exists(tree_id)
    }

    fn ids(&self) -> Result<Vec<String>, TreeStoreError> {
        self.inner.ids()
    }

    fn save(&self, tree: &ProductAgentTree) -> Result<Revision, TreeStoreError> {
        if self.refuse.load(Ordering::SeqCst)
            || (self.refuse_sealed.load(Ordering::SeqCst) && !tree.candidates().is_empty())
            || (self.refuse_seal_failed.load(Ordering::SeqCst)
                && tree.attempts().values().any(|attempt| {
                    attempt
                        .failure
                        .as_ref()
                        .is_some_and(|failure| failure.kind == "seal_failed")
                }))
        {
            return Err(TreeStoreError::unavailable("injected tree store refusal"));
        }
        self.inner.save(tree)
    }

    fn load(&self, tree_id: &str) -> Result<Option<PersistedTree>, TreeStoreError> {
        self.inner.load(tree_id)
    }
}

pub(crate) struct Fixture {
    root: tempfile::TempDir,
    tree_id: String,
    runtime: Arc<ProductTopologyRuntime>,
    parent: SpawnParent,
    evidence: PathBuf,
    store: Arc<SwitchableStore>,
    vcs: Arc<crate::occupancy_vcs::fake::FakeVcs>,
    failure_kind: ModelErrorKind,
    _binding: crate::agent::injected_child_binding::Guard,
}

#[tokio::test]
async fn sql_deleted_cascade_views_rebuild_through_real_reopen_in_postorder() {
    use crate::agent_tree::{AgentCommand, AgentCommandKind};
    use crate::agent_tree_protocol::AttemptScopedRequest;
    use plakous_recursive_subagents_store::SqliteTreeStore;

    async fn terminal(fixture: &Fixture, id: &str) -> TreeFeed {
        let command = format!("deleted:{id}");
        fixture
            .runtime
            .mutate_then_emit(false, |tree| {
                tree.record_command(AgentCommand::intent(
                    &command,
                    id,
                    tree.attempt(id).map(|attempt| attempt.attempt_id.clone()),
                    AgentCommandKind::Start,
                    "fail".to_owned(),
                    Vec::new(),
                ))
                .map_err(|error| error.message)
            })
            .unwrap();
        let sessions = fixture.sessions();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                spawn_child_operation_id(&fixture.tree_id, id).unwrap(),
                fixture.binding(),
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        let binding = session.locator();
        fixture
            .runtime
            .mutate_then_emit(false, |tree| {
                tree.record_child_session(id, &binding.to_string())
                    .map_err(|error| error.message)
            })
            .unwrap();
        session
            .agent()
            .start(CommandId::new(command).unwrap(), "fail")
            .await
            .unwrap()
            .result()
            .unwrap()
            .clone()
            .result()
            .await
            .unwrap();
        let locator = session.release().await.unwrap();
        read_tree_feed(&sessions, locator.session_id(), 0)
            .await
            .unwrap()
    }

    let fixture =
        Fixture::with_failure_kind("deleted-cascade-reopen", ModelErrorKind::InvalidRequest);
    let (parent, child, sessionless) = fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            let mut snapshot = tree.snapshot.clone();
            snapshot.max_depth = 2;
            snapshot.digest = snapshot.compute_digest();
            tree.install_snapshot(snapshot);
            let parent = tree
                .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                .unwrap()
                .agent_id;
            let child = tree
                .spawn_agent(&parent, &SpawnAgentRequest::test_review(), None)
                .unwrap()
                .agent_id;
            let sessionless = tree
                .spawn_agent(&parent, &SpawnAgentRequest::test_review(), None)
                .unwrap()
                .agent_id;
            tree.close_agent(
                &parent,
                &AttemptScopedRequest {
                    agent_id: sessionless.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap();
            Ok((parent, child, sessionless))
        })
        .unwrap();
    let child_feed = terminal(&fixture, &child).await;
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            child_feed
                .apply(tree, &child)
                .map(|_| ())
                .map_err(|error| error.message)
        })
        .unwrap();
    let parent_feed = terminal(&fixture, &parent).await;
    let mut live = fixture.runtime.read(ProductAgentTree::clone).unwrap();
    parent_feed.apply(&mut live, &parent).unwrap();
    assert!(
        ProductAgentTree::reopen(live.to_persisted())
            .session_recovery_targets()
            .is_empty(),
        "ordinary reopen must not select stopped or fresh sessionless nodes"
    );
    let database = fixture.root.path().join("tree.sqlite3");
    let store = Arc::new(SqliteTreeStore::open(&database).unwrap());
    fixture
        .runtime
        .read(|tree| {
            let mut unpublished = tree.clone();
            unpublished.acknowledge_revision(Revision::published(0));
            let _ = store.save(&unpublished).unwrap();
        })
        .unwrap();
    crate::topology_runtime::force_release(&fixture.tree_id).await;
    rusqlite::Connection::open(&database).unwrap().execute_batch(
        "UPDATE agent_nodes SET fold_order=NULL;
         UPDATE agent_nodes SET status='running',active_turn=0,session_locator=NULL WHERE parent_agent_id IS NOT NULL;
         UPDATE agent_attempts SET status='running',output_json=NULL,candidate_id=NULL,turn_id=NULL,ended_at=NULL,
             blocked_kind=NULL,blocked_account=NULL,blocked_resets_at=NULL,failure_kind=NULL,failure_message=NULL,
             turns=0,tool_calls=0,input_tokens=0,output_tokens=0,submit_harness_nudge=0;
         UPDATE agent_commands SET fact=NULL,turn_id=NULL,accepted_seq=NULL;
         DELETE FROM agent_feed_cursors;",
    ).unwrap();
    let deleted = store.load(&fixture.tree_id).unwrap().unwrap();
    assert!(
        deleted
            .rows()
            .operations()
            .contains_key(&format!("child_session:{child}"))
    );
    assert!(
        deleted
            .rows()
            .operations()
            .contains_key(&format!("child_session:{parent}"))
    );
    let runtime = crate::topology_runtime::insert_root_with(
        fixture.tree_id.clone(),
        ProductAgentTree::reopen(deleted),
        store.clone(),
        None,
    )
    .unwrap();
    runtime.remember_rejoin_spec(Arc::new(fixture.parent.clone()));
    let targets = runtime
        .read(ProductAgentTree::session_recovery_targets)
        .unwrap();
    assert!(
        targets.iter().position(|id| id == &parent).unwrap()
            > targets.iter().position(|id| id == &child).unwrap()
    );
    assert!(targets.contains(&sessionless));
    // This is the product's real reopen recovery entry, not explicit fold calls.
    let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.recovered.len(), 3);
    assert_eq!(
        runtime
            .read(ProductAgentTree::session_recovery_targets)
            .unwrap(),
        [] as [String; 0]
    );
    // Exact opens add release/head records. Advance the no-deletion oracle to
    // the same authoritative tails before comparing its derived rows.
    for id in [&child, &parent] {
        let locator = runtime
            .read(|tree| tree.node(id).unwrap().session_locator.clone().unwrap())
            .unwrap();
        let locator: SessionLocator = locator.parse().unwrap();
        let feed = read_tree_feed(
            &fixture.sessions(),
            locator.session_id(),
            live.feed_cursor(&locator.session_id().to_string()),
        )
        .await
        .unwrap();
        feed.apply(&mut live, id).unwrap();
    }
    runtime
        .read(|rebuilt| {
            for id in [&child, &parent, &sessionless] {
                assert_eq!(rebuilt.node(id).unwrap(), live.node(id).unwrap());
                assert_eq!(rebuilt.attempt(id).unwrap(), live.attempt(id).unwrap());
            }
            assert_eq!(rebuilt.messages(), live.messages());
            assert_eq!(rebuilt.operations(), live.operations());
            assert_eq!(rebuilt.commands(), live.commands());
        })
        .unwrap();
    let again = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert!(again.recovered.is_empty() && again.refused.is_empty());
    assert!(
        fixture
            .sessions()
            .accepted_create(&spawn_child_operation_id(&fixture.tree_id, &sessionless).unwrap())
            .await
            .unwrap()
            .is_none()
    );
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn acknowledged_queued_effect_settles_once_when_feed_is_refused() {
    use crate::occupancy_vcs::fake::FakeOp;
    let fixture = Fixture::new("refused-feed-queued-effect");
    let child = fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "implement", "work")
                    .with_fork(),
                None,
            )
            .map(|spawned| spawned.agent_id)
            .map_err(|error| error.message)
        })
        .unwrap();
    let driver = fixture.runtime.driver();
    {
        let mut tree = driver.tree_handle_for_test().lock().unwrap();
        tree.record_host_cancellation(&child).unwrap();
        driver.save(&mut tree).unwrap();
        driver
            .fold_and_commit(&mut tree, |tree| tree.fold_due(&child))
            .unwrap();
        assert!(tree.owes_effects());
        assert_eq!(tree.revision, tree.stored_revision);
    }
    let feed = TreeFeed {
        session_id: fixture.tree_id.clone(),
        retained_from: 1,
        below_retained: None,
        heads: Vec::new(),
        head_turns: std::collections::BTreeMap::default(),
        records: Vec::new(),
        reads: crate::agent_tree::TreeFeedReads::default(),
    };
    assert_eq!(fixture.vcs.calls_of(FakeOp::Seal).len(), 0);
    assert!(
        super::super::feed_projection::apply_on_driver(driver, &feed, "missing-agent").is_err()
    );
    assert_eq!(
        fixture.vcs.calls_of(FakeOp::Seal).len(),
        1,
        "a refused feed must not drop earlier acknowledged tickets"
    );
    assert!(
        super::super::feed_projection::apply_on_driver(driver, &feed, "missing-agent").is_err()
    );
    assert_eq!(fixture.vcs.calls_of(FakeOp::Seal).len(), 1);
    assert!(!driver.read(ProductAgentTree::owes_effects).unwrap());
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn stale_incremental_feed_retries_once_from_start_without_losing_wait_evidence() {
    use super::super::feed_projection::{apply_on_runtime, retry_tree_feed};
    use crate::agent_tree::{AgentCommand, AgentCommandKind, TreeProjectorError};
    let fixture =
        Fixture::with_failure_kind("stale-incremental-feed", ModelErrorKind::InvalidRequest);
    let id = fixture.spawn();
    let sessions = fixture.sessions();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("created session required")
    };
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_child_session(&id, &session.locator().to_string())
                .unwrap();
            tree.record_command(AgentCommand::intent(
                "stale-feed-start",
                &id,
                tree.attempt(&id).map(|attempt| attempt.attempt_id.clone()),
                AgentCommandKind::Start,
                "fail",
                Vec::new(),
            ))
            .unwrap();
            Ok(())
        })
        .unwrap();
    session
        .agent()
        .start(CommandId::new("stale-feed-start").unwrap(), "fail")
        .await
        .unwrap()
        .result()
        .unwrap()
        .clone()
        .result()
        .await
        .unwrap();
    let locator = session.release().await.unwrap();
    let full = read_tree_feed(&sessions, locator.session_id(), 0)
        .await
        .unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.project_feed(&id, &full.session_id, &full.records[..1], &full.reads)
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .unwrap();
    let mut stale = read_tree_feed(&sessions, locator.session_id(), 1)
        .await
        .unwrap();
    stale
        .reads
        .insert_waits(locator, vec![("carried-wait".to_owned(), true)]);
    let mut expected_reads = full.reads.clone();
    expected_reads.extend_waits_from(&stale.reads);
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.clear_feed_views();
            Ok(())
        })
        .unwrap();
    let mut calls = 0;
    let (projection, _) = retry_tree_feed(&sessions, stale.clone(), |feed| {
        calls += 1;
        if calls == 2 {
            assert_eq!(feed.records.first().unwrap().header().seq.get(), 1);
            assert_eq!(feed.reads, expected_reads);
        }
        apply_on_runtime(&fixture.runtime, feed, &id)
    })
    .await
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(
        projection.cursor,
        full.records.last().unwrap().header().seq.get()
    );
    fixture
        .runtime
        .read(|tree| {
            assert_eq!(tree.attempt(&id).unwrap().usage.turns, 1);
            assert!(tree.node(&id).unwrap().fold_order.is_some());
        })
        .unwrap();
    let mut calls = 0;
    let refused = retry_tree_feed(&sessions, stale, |_| {
        calls += 1;
        Err::<(), _>(TreeProjectorError::RefoldRequired)
    })
    .await
    .unwrap_err();
    assert_eq!(calls, 2, "a second refusal never spins");
    assert_eq!(refused.code, TopologyErrorCode::OperationRecoveryRequired);
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn explicit_deleted_fold_rebuilds_in_the_existing_recovery_pass() {
    use crate::agent_tree::{AgentCommand, AgentCommandKind};
    let fixture = Fixture::with_failure_kind("live-deleted-fold", ModelErrorKind::InvalidRequest);
    let id = fixture.spawn();
    let sessions = fixture.sessions();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("created session required")
    };
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_child_session(&id, &session.locator().to_string())
                .unwrap();
            tree.record_command(AgentCommand::intent(
                "live-rebuild-start",
                &id,
                tree.attempt(&id).map(|attempt| attempt.attempt_id.clone()),
                AgentCommandKind::Start,
                "fail",
                Vec::new(),
            ))
            .unwrap();
            Ok(())
        })
        .unwrap();
    session
        .agent()
        .start(CommandId::new("live-rebuild-start").unwrap(), "fail")
        .await
        .unwrap()
        .result()
        .unwrap()
        .clone()
        .result()
        .await
        .unwrap();
    let locator = session.release().await.unwrap();
    let feed = read_tree_feed(&sessions, locator.session_id(), 0)
        .await
        .unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            feed.apply(tree, &id)
                .map(|_| ())
                .map_err(|error| error.message)
        })
        .unwrap();
    let mut expected = fixture.runtime.read(ProductAgentTree::clone).unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.clear_feed_views();
            Ok(())
        })
        .unwrap();
    let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert!(report.recovered.contains(&id));
    let final_feed = read_tree_feed(
        &sessions,
        locator.session_id(),
        expected.feed_cursor(&locator.session_id().to_string()),
    )
    .await
    .unwrap();
    final_feed.apply(&mut expected, &id).unwrap();
    fixture
        .runtime
        .read(|rebuilt| {
            assert_eq!(rebuilt.node(&id).unwrap(), expected.node(&id).unwrap());
            assert_eq!(
                rebuilt.attempt(&id).unwrap(),
                expected.attempt(&id).unwrap()
            );
            assert_eq!(rebuilt.messages(), expected.messages());
            assert_eq!(rebuilt.operations(), expected.operations());
            assert_eq!(rebuilt.commands(), expected.commands());
        })
        .unwrap();
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn interactive_exit_batches_real_root_and_child_leases_without_settling() {
    use plakous_offline_crash_harness::script::ScriptPlan;
    use plakous_offline_crash_harness::scripted_runtime_binding;

    let fixture = Fixture::new("exit-live-leases");
    let sessions = fixture.sessions();
    let script = Arc::new(
        serde_json::from_value::<ScriptPlan>(json!({
            "version": 1,
            "stall_ignores_cancellation": true,
            "steps": [{"stall_ms": 2000, "output": {"text": "late actor result"}}]
        }))
        .unwrap(),
    );
    let root = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("exit-root").unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap();
    let CreateSessionOutcome::Created { session: root, .. } = root else {
        panic!("created root required")
    };
    let root_agent = root.agent().clone();
    let locator = root.locator();
    let mut actors = vec![root_agent];
    let mut results = Vec::new();
    for _ in 0..2 {
        let id = fixture.spawn();
        let evidence = fixture.root.path().join(format!("{id}.evidence.jsonl"));
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        let binding =
            scripted_runtime_binding(fixture.root.path(), controller, Arc::clone(&script)).unwrap();
        let created = sessions
            .create(CreateSessionRequest::new(
                spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
                binding,
            ))
            .await
            .unwrap();
        let CreateSessionOutcome::Created { session, .. } = created else {
            panic!("created child required")
        };
        let agent = session.agent().clone();
        let submission = agent
            .start(CommandId::new(format!("live:{id}")).unwrap(), "stall")
            .await
            .unwrap();
        let result = submission.result().unwrap().clone();
        let turn = result.turn_id();
        let child_locator = session.locator();
        fixture.bind_turn(&id, &child_locator, turn);
        let control = agent.turn_control(turn).unwrap();
        assert!(
            fixture
                .runtime
                .try_occupy_live(
                    id,
                    Box::new(crate::agent::live::ProductLiveChild::new(
                        AttachedCodingSession {
                            output_bound_rederived: None,
                            reachability_probe: None,
                            sessions: Arc::clone(&sessions),
                            opened: Some(session),
                            recovered_turn: None,
                            locator: child_locator,
                            topology_owner: None,
                        },
                        turn,
                        control
                    ))
                )
                .await
                .is_ok()
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while !std::fs::read(&evidence).is_ok_and(|bytes| !bytes.is_empty()) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        actors.push(agent);
        results.push(result);
    }
    // Abort recovery before any child grant can be reacquired.
    fixture
        .runtime
        .start_recovery_pass(|| tokio::spawn(std::future::pending()));
    let root = AttachedCodingSession {
        output_bound_rederived: None,
        reachability_probe: None,
        sessions: Arc::clone(&sessions),
        opened: Some(root),
        recovered_turn: None,
        locator,
        topology_owner: Some(fixture.tree_id.clone()),
    };
    let db = rusqlite::Connection::open(fixture.database()).unwrap();
    let mutations = || {
        db.query_row::<i64, _, _>("SELECT semantic_mutations FROM store_meta", [], |row| {
            row.get(0)
        })
        .unwrap()
    };
    let active = || {
        db.query_row::<i64, _, _>(
            "SELECT count(*) FROM branches WHERE active_owner IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(active(), 3);
    let before = mutations();
    let started = std::time::Instant::now();
    tokio::time::timeout(
        Duration::from_secs(1),
        root.abandon_and_release(Duration::from_millis(400)),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(fixture.runtime.is_closed());
    assert!(!fixture.runtime.recovery_pass_running());
    assert_eq!(active(), 0);
    assert_eq!(
        mutations(),
        before + 1,
        "one transaction for root and both children"
    );
    for agent in &actors {
        assert!(
            agent
                .start(CommandId::new("after-exit").unwrap(), "not admitted")
                .await
                .is_err()
        );
    }
    for result in results {
        let _ = tokio::time::timeout(Duration::from_secs(10), result.result())
            .await
            .unwrap();
    }
    assert_eq!(
        mutations(),
        before + 1,
        "late actors cannot publish after release"
    );
}

struct LateReleaseChild {
    sessions: Arc<AgentSessions>,
    session: OpenedSession,
    turn: plakous::model::TurnId,
    entered: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
}

impl crate::topology_runtime::LiveChildHandle for LateReleaseChild {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        Some(self.session.agent().clone())
    }
    fn turn_id(&self) -> plakous::model::TurnId {
        self.turn
    }
    fn turn_control(&self) -> plakous::agent::TurnControl {
        panic!("unused fixture control")
    }
    fn follow_up(
        &self,
        _: CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Err("unused".into()) })
    }
    fn restart(
        &mut self,
        _: crate::agent_tree_tools::TreeDriver,
        _: CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>>
    {
        Box::pin(async { Err("unused".into()) })
    }
    fn release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.finish.notified().await;
            match self.session.release().await {
                Ok(locator) => {
                    let (cursor, recorded) =
                        super::super::live::release_observation(&driver, locator);
                    super::super::live::released_with_feed(
                        &self.sessions,
                        locator,
                        None,
                        cursor,
                        recorded,
                    )
                    .await
                }
                Err(error) => crate::topology_runtime::LiveRelease {
                    feed: None,
                    locator: error.released_locator().map(|locator| locator.to_string()),
                    failure: Some(error.to_string()),
                },
            }
        })
    }
    fn cancel_and_release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        self.release(driver)
    }
}

#[tokio::test]
async fn panicking_child_create_frees_startup_owner_and_triggers_recovery() {
    use crate::agent_tree_tools::{SpawnBindError, SpawnStart, SpawnedSessionBinder};

    let fixture = Fixture::new("startup-create-panic");
    let start = fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.pin_bind_hints_for_test(crate::orchestration::BindHints::from_names(
                &["claude"],
                &[],
            ));
            let binding = crate::orchestration::RosterBinding::leaf("claude", "test-model");
            tree.pin_template("review", binding);
            let spawned = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map_err(|error| error.message)?;
            let node = tree.node(&spawned.agent_id).unwrap();
            Ok(SpawnStart {
                description: node.description.clone(),
                prompt: node.prompt.clone(),
                output_schema_json: node.output_schema_json.clone(),
                turn_token: tree.attempt(&spawned.agent_id).unwrap().turn_token,
                path: node.path.display().to_string(),
                tier: node.tier,
                account: node.account.clone(),
                tuple_id: node.tuple_id.clone(),
                effort: node.effort.clone(),
                spawned,
            })
        })
        .unwrap();
    let id = start.spawned.agent_id.clone();
    let factory = crate::agent::injected_child_binding::lookup_for_test().unwrap();
    let panic_binding = crate::agent::injected_child_binding::install(Arc::new(|_, _, _| {
        panic!("injected create panic payload must not be persisted")
    }));
    let recovered = Arc::new(tokio::sync::Notify::new());
    let notified = recovered.clone();
    fixture
        .runtime
        .set_retirement_recovery(Arc::new(move || notified.notify_one()))
        .await;
    let binder = crate::agent::children::ProductChildSessionBinder {
        parent: fixture.parent.clone(),
    };
    let result = binder.bind(start).await;
    assert!(
        matches!(result, Err(SpawnBindError::RecoveryRequired { .. })),
        "{result:?}"
    );
    tokio::time::timeout(Duration::from_secs(1), recovered.notified())
        .await
        .expect("panic cleanup must trigger recovery");
    assert!(!fixture.runtime.live.lock().await.contains_key(&id));
    fixture
        .runtime
        .read(|tree| {
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
            assert_eq!(
                tree.attempt(&id).unwrap().failure.as_ref().unwrap().message,
                "child startup owner ended without finishing"
            );
        })
        .unwrap();
    drop(panic_binding);
    let _restore = crate::agent::injected_child_binding::install(factory);
    let port = ParentSessionPort {
        parent: fixture.parent.clone(),
    };
    match recover_child(&fixture.runtime, &port, &id, None).await {
        Ok(receipt) => assert!(receipt.recovered),
        Err(error) => assert_ne!(error.code, TopologyErrorCode::RecoveryOwnerLive),
    }
}

#[tokio::test]
async fn late_release_allows_in_process_recover_agent_without_held_lease() {
    late_release_recovery(false, false).await;
}

#[tokio::test]
async fn submitted_late_release_settles_completed_in_background() {
    late_release_recovery(true, false).await;
}

#[tokio::test]
async fn installed_retirement_recovery_starts_a_pass_after_late_release() {
    late_release_recovery(true, true).await;
}

async fn late_release_recovery(submitted: bool, installed: bool) {
    let fixture = Fixture::new("late-real-release");
    let id = fixture.spawn();
    let CreateSessionOutcome::Created { session, .. } = fixture
        .sessions()
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("expected created");
    };
    let turn = "00000000-0000-0000-0000-000000000001".parse().unwrap();
    fixture.bind_turn(&id, &session.locator(), turn);
    let (reports, mut report_rx) = tokio::sync::watch::channel(None);
    if submitted {
        fixture
            .runtime
            .mutate_then_emit(false, |tree| {
                let token = tree.attempt(&id).unwrap().turn_token;
                tree.submit_result(
                    &id,
                    &crate::agent_tree_protocol::SubmitResultRequest {
                        turn_token: token,
                        output_json: r#"{"ok":true}"#.into(),
                    },
                    None,
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
        if installed {
            install_retirement_recovery(&fixture.runtime, &fixture.tree_id).await;
        } else {
            let tree_id = fixture.tree_id.clone();
            fixture
                .runtime
                .set_retirement_recovery(Arc::new(move || {
                    let reports = reports.clone();
                    start_tree_recovery_pass(tree_id.clone(), move |report| {
                        reports.send_replace(report);
                    });
                }))
                .await;
        }
    }
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        fixture
            .runtime
            .try_occupy_live(
                id.clone(),
                Box::new(LateReleaseChild {
                    sessions: fixture.sessions(),
                    session,
                    turn,
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    let runtime = fixture.runtime.clone();
    let child = id.clone();
    let caller = tokio::spawn(async move { runtime.cancel_live(&child).await });
    entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(
        plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT
            + plakous_host_protocol::limits::HOST_SESSION_RELEASE_TIMEOUT
            + Duration::from_secs(1),
    )
    .await;
    assert!(caller.await.unwrap().is_err());
    let port = ParentSessionPort {
        parent: fixture.parent.clone(),
    };
    assert_eq!(
        recover_child(&fixture.runtime, &port, &id, None)
            .await
            .unwrap_err()
            .code,
        TopologyErrorCode::RecoveryOwnerLive
    );
    tokio::time::resume();
    let pass_lock = if installed {
        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture.runtime.recovery_pass_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Some(fixture.runtime.lock_reattach().await)
    } else {
        None
    };
    let runtime = fixture.runtime.clone();
    let child = id.clone();
    let joined = tokio::spawn(async move { runtime.cancel_live(&child).await });
    finish.notify_one();
    joined.await.unwrap().unwrap();
    if installed {
        assert!(
            fixture.runtime.recovery_pass_running(),
            "the installed callback must start a new pass after the late release"
        );
        drop(pass_lock);
        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture
                .runtime
                .read(|tree| tree.attempt(&id).unwrap().status)
                .unwrap()
                != AttemptStatus::Completed
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    if submitted {
        if !installed {
            loop {
                if report_rx
                    .borrow_and_update()
                    .as_ref()
                    .is_some_and(|report| report.recovered.contains(&id))
                {
                    break;
                }
                report_rx.changed().await.unwrap();
            }
        }
        fixture
            .runtime
            .read(|tree| {
                assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Completed);
                assert_eq!(
                    tree.attempt(&id).unwrap().output_json.as_deref(),
                    Some(r#"{"ok":true}"#)
                );
            })
            .unwrap();
        return;
    }
    let _lock = fixture.runtime.lock_reattach().await;
    let receipt = recover_child(&fixture.runtime, &port, &id, None)
        .await
        .unwrap();
    assert!(receipt.recovered);
    assert_eq!(receipt.attempt, Some(AttemptStatus::Failed));
}

#[tokio::test]
async fn abandonment_during_late_release_retains_real_agent_capability() {
    let fixture = Fixture::new("late-abandon-agent");
    let id = fixture.spawn();
    let CreateSessionOutcome::Created { session, .. } = fixture
        .sessions()
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("expected created");
    };
    let agent = session.agent().clone();
    let turn = "00000000-0000-0000-0000-000000000001".parse().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        fixture
            .runtime
            .try_occupy_live(
                id.clone(),
                Box::new(LateReleaseChild {
                    sessions: fixture.sessions(),
                    session,
                    turn,
                    entered: entered.clone(),
                    finish,
                })
            )
            .await
            .is_ok()
    );
    let runtime = fixture.runtime.clone();
    let caller = tokio::spawn(async move { runtime.cancel_live(&id).await });
    entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(
        plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT
            + plakous_host_protocol::limits::HOST_SESSION_RELEASE_TIMEOUT
            + Duration::from_secs(1),
    )
    .await;
    assert!(caller.await.unwrap().is_err());
    let agents = crate::topology_runtime::abandon_live_agents(&fixture.tree_id).await;
    assert!(agents.iter().any(|owned| owned.id() == agent.id()));
    tokio::time::resume();
    plakous::sessions::abandon_and_release(&agents, Duration::from_millis(200))
        .await
        .unwrap();
}

impl Fixture {
    pub(crate) fn new(tree_id: &str) -> Self {
        Self::with_failure_kind(tree_id, ModelErrorKind::OutcomeUnknown)
    }

    fn with_failure_kind(tree_id: &str, failure_kind: ModelErrorKind) -> Self {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join("evidence.jsonl");
        let workspace = root.path().to_path_buf();
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        let binding = crate::agent::injected_child_binding::install(Arc::new(move |_, _, _| {
            projected_terminal_failure_runtime_binding(&workspace, controller.clone(), failure_kind)
                .map_err(|error| error.to_string())
        }));
        let vcs = Arc::new(crate::occupancy_vcs::fake::FakeVcs::new());
        let tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            tree_id,
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let store = Arc::new(SwitchableStore::default());
        let runtime = crate::topology_runtime::insert_root_with(
            tree_id,
            tree,
            Arc::clone(&store) as Arc<dyn TreeStore>,
            None,
        )
        .unwrap();
        let parent = SpawnParent::Api(ApiTopologyParent {
            created_output_bound: None,
            prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
            product_home: None,
            tool_selection: crate::tool_plan::NodeToolSelection::default(),
            workspace_root: root.path().to_path_buf(),
            cwd: root.path().to_path_buf(),
            plan_state_authority: tree_id.into(),
            composition: CompositionSelection::Direct,
            topology_tree_id: Some(tree_id.into()),
            topology_caller_id: None,
            state_roots: ProductStateRoots::with_doubles(
                root.path().join("plak"),
                root.path().join("state"),
            ),
            model: "vendor/parent".into(),
            effort: None,
            target: crate::binding::ApiTarget::OpenRouter {
                wire: ResolvedProtocol::Chat,
            },
            scheduled_compaction: true,
            max_model_rounds: NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_enabled: true,
        });
        std::fs::create_dir_all(root.path().join("plak")).unwrap();
        runtime.remember_rejoin_spec(Arc::new(parent.clone()));
        Self {
            root,
            tree_id: tree_id.to_owned(),
            runtime,
            parent,
            evidence,
            store,
            vcs,
            failure_kind,
            _binding: binding,
        }
    }

    fn database(&self) -> PathBuf {
        self.root.path().join("plak").join("runtime.sqlite3")
    }

    fn sessions(&self) -> Arc<AgentSessions> {
        self.parent.state_roots().runtime_sessions().unwrap()
    }

    fn spawn(&self) -> String {
        self.runtime
            .mutate_then_emit(false, |tree| {
                tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map(|spawned| spawned.agent_id)
                    .map_err(|error| error.message)
            })
            .unwrap()
    }

    async fn stuck_fork(&self) -> String {
        let id = self
            .runtime
            .mutate_then_emit(false, |tree| {
                tree.spawn_agent(
                    "root",
                    &SpawnAgentRequest::test_named("implement", Tier::Mutate, "implement", "work")
                        .with_fork(),
                    None,
                )
                .map(|spawned| spawned.agent_id)
                .map_err(|error| error.message)
            })
            .unwrap();
        let sessions = self.sessions();
        let (session, handle, _) = self.start_failing_turn(&sessions, &id).await;
        Self::await_unknown_outcome(&handle).await;
        self.bind_turn(&id, &session.locator(), handle.turn_id());
        session.release().await.unwrap();
        self.runtime
            .mutate_then_emit(false, |tree| {
                tree.record_host_retirement(&id, "lost owner".into())
                    .and_then(|value| tree.fold_due(&id).map(|_| value))
                    .map_err(|error| error.message)
            })
            .unwrap();
        id
    }

    fn binding(&self) -> SessionRuntimeBinding {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &self.evidence)
                .unwrap();
        projected_terminal_failure_runtime_binding(self.root.path(), controller, self.failure_kind)
            .unwrap()
    }

    /// Creates `id`'s session under the spawn's own create identity and
    /// starts the turn that ends with an unknown outcome.
    async fn start_failing_turn(
        &self,
        sessions: &Arc<AgentSessions>,
        id: &str,
    ) -> (
        OpenedSession,
        plakous::agent::TurnResultHandle,
        plakous::agent::TurnControl,
    ) {
        let release = self.root.path().join(format!("release-{id}"));
        let controller = BoundaryController::quiet_gated_from_evidence_path(
            CrashBoundary::ProviderDispatchOne,
            &self.evidence,
            &release,
        )
        .unwrap();
        let binding = projected_terminal_failure_runtime_binding(
            self.root.path(),
            controller,
            ModelErrorKind::OutcomeUnknown,
        )
        .unwrap();
        let operation_id = spawn_child_operation_id(&self.tree_id, id).unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(operation_id, binding))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        let submission = session
            .agent()
            .start(
                CommandId::new(format!("stuck:{id}")).unwrap(),
                "fail with an unknown outcome",
            )
            .await
            .unwrap();
        let handle = submission.result().unwrap().clone();
        // Capture the active capability before the fast unknown-outcome
        // fixture can retire it; callers retain it through waiter cleanup.
        let control = session.agent().turn_control(handle.turn_id()).unwrap();
        std::fs::write(release, "release").unwrap();
        (session, handle, control)
    }

    async fn quiescent_after_reopen(&self, locator: SessionLocator) -> SessionLocator {
        let OpenSessionOutcome::Opened { session, .. } = self
            .sessions()
            .open(OpenSessionRequest::new(
                SessionOperationId::new(format!(
                    "quiet:{}:{}",
                    self.tree_id,
                    locator.checkpoint_id()
                ))
                .unwrap(),
                locator,
                self.binding(),
            ))
            .await
            .unwrap()
        else {
            panic!("local reopen must abandon the old uncertainty");
        };
        session.release().await.unwrap()
    }

    async fn await_unknown_outcome(handle: &plakous::agent::TurnResultHandle) {
        let terminal = handle.clone().result().await.unwrap();
        assert!(
            matches!(terminal.outcome(), TurnResultOutcome::RecoveryRequired(_)),
            "{:?}",
            terminal.outcome()
        );
    }

    fn bind_turn(&self, id: &str, locator: &SessionLocator, turn: plakous::model::TurnId) {
        self.runtime
            .mutate_then_emit(false, |tree| {
                tree.bind_session_locator(id, locator.to_string())
                    .map_err(|error| error.message)?;
                tree.bind_attempt_turn(id, turn.to_string())
                    .map_err(|error| error.message)?;
                tree.set_active_turn(id, true)
                    .map_err(|error| error.message)
            })
            .unwrap();
    }

    /// The stranding failure mode: a child's turn ends with an unknown
    /// outcome, its live owner is the turn waiter, and the session store
    /// refuses the waiter's own recycle reopen, so the turn is never
    /// abandoned and the node is left `recovery_required`.
    pub(crate) async fn stuck_child(&self) -> String {
        let id = self.spawn();
        let sessions = self.sessions();
        let (session, handle, control) = self.start_failing_turn(&sessions, &id).await;
        let turn = handle.turn_id();
        Self::await_unknown_outcome(&handle).await;
        let locator = session.locator();
        self.bind_turn(&id, &locator, turn);
        let live = crate::agent::live::ProductLiveChild::new(
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
        assert!(
            self.runtime
                .try_occupy_live(id.clone(), Box::new(live))
                .await
                .is_ok()
        );
        refuse_checkpoints(&self.database());
        spawn_topology_turn_waiter(
            crate::agent::children::TurnWaiterRegistration::register(self.runtime.driver(), &id)
                .unwrap(),
            Arc::clone(&self.runtime.live),
            None,
            None,
            handle,
        );
        assert!(
            self.has_turn_waiter(&id),
            "the waiter registers before it is spawned"
        );
        tokio::time::timeout(Duration::from_secs(60), async {
            // The waiter's registration ends with its task.
            while self.runtime.live.lock().await.contains_key(&id)
                || self.status(&id) != NodeStatus::RecoveryRequired
                || self.has_turn_waiter(&id)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the waiter did not settle: {:?} {:?}",
                self.status(&id),
                self.failure_kind(&id)
            )
        });
        assert_eq!(
            self.failure_kind(&id).as_deref(),
            Some("turn_recovery_required")
        );
        admit_checkpoints(&self.database());
        id
    }

    fn has_turn_waiter(&self, id: &str) -> bool {
        self.runtime.read(|tree| tree.has_turn_waiter(id)).unwrap()
    }

    pub(crate) fn status(&self, id: &str) -> NodeStatus {
        self.runtime
            .read(|tree| tree.node(id).unwrap().presentation_status(tree.state))
            .unwrap()
    }

    fn attempt_status(&self, id: &str) -> AttemptStatus {
        self.runtime
            .read(|tree| tree.attempt(id).unwrap().status)
            .unwrap()
    }

    pub(crate) fn failure_kind(&self, id: &str) -> Option<String> {
        self.runtime
            .read(|tree| {
                tree.attempt(id)
                    .and_then(|attempt| attempt.failure.as_ref())
                    .map(|failure| failure.kind.clone())
            })
            .unwrap()
    }

    fn locator(&self, id: &str) -> Option<String> {
        self.runtime
            .read(|tree| tree.node(id).unwrap().session_locator.clone())
            .unwrap()
    }

    fn snapshot(&self, id: &str) -> (u64, crate::agent_tree::NodeRecord, Option<AttemptStatus>) {
        self.runtime
            .read(|tree| {
                (
                    tree.revision,
                    tree.node(id).unwrap().clone(),
                    tree.attempt(id).map(|attempt| attempt.status),
                )
            })
            .unwrap()
    }

    fn dispatches(&self) -> usize {
        std::fs::read_to_string(&self.evidence)
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                    == "provider_dispatched"
            })
            .count()
    }

    fn live_nodes(&self) -> usize {
        self.runtime
            .read(|tree| {
                tree.nodes()
                    .values()
                    .filter(|node| node.status != NodeStatus::Closed)
                    .count()
            })
            .unwrap()
    }

    /// A topology tool through the product's own ports, exactly as the idle
    /// `invoke_tool` and a model's call reach it.
    async fn invoke(&self, name: &str, arguments: serde_json::Value) -> (bool, serde_json::Value) {
        let receipt = crate::agent::invoke_topology_tool_on_hub(
            &self.tree_id,
            None,
            name,
            &arguments,
            &format!("{name}:{}", recovery_operation_kind()),
        )
        .await
        .unwrap();
        (
            receipt.succeeded,
            serde_json::from_str(&receipt.detail)
                .unwrap_or(serde_json::Value::String(receipt.detail)),
        )
    }

    async fn recover_agent(&self, id: &str) -> (bool, serde_json::Value) {
        self.invoke("recover_agent", json!({ "agent_id": id }))
            .await
    }

    fn admit_followup(&self, id: &str) {
        self.runtime
            .mutate_then_emit(false, |tree| {
                tree.send_agent_message(
                    "root",
                    &SendAgentMessageRequest {
                        agent_id: id.to_owned(),
                        message: "again".into(),
                        purpose: Some(MessagePurpose::Followup),
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .map(|_| ())
                .map_err(|error| error.message)
            })
            .unwrap();
    }
}

fn code(refusal: &serde_json::Value) -> &str {
    refusal["code"].as_str().unwrap_or_default()
}

fn refuse_checkpoints(database: &Path) {
    rusqlite::Connection::open(database)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_recovery BEFORE INSERT ON checkpoints BEGIN \
             SELECT RAISE(FAIL, 'injected checkpoint refusal'); END;",
        )
        .unwrap();
}

/// Makes the branch's last released lease active again, as if a holder in
/// another process still had it. It expires on its original clock.
fn reinstate_released_lease(database: &Path) {
    let changed = rusqlite::Connection::open(database)
        .unwrap()
        .execute(
            "UPDATE branches SET active_owner=released_owner,active_fence=released_fence,\
             active_time=released_time,active_ns=released_ns,\
             active_duration=released_duration,released_owner=NULL,released_fence=NULL,\
             released_time=NULL,released_ns=NULL,released_duration=NULL \
             WHERE released_owner IS NOT NULL",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1, "one released lease to reinstate");
}

fn release_reinstated_lease(database: &Path) {
    let changed = rusqlite::Connection::open(database)
        .unwrap()
        .execute(
            "UPDATE branches SET released_owner=active_owner,released_fence=active_fence,\
             released_time=active_time,released_ns=active_ns,\
             released_duration=active_duration,active_owner=NULL,active_fence=NULL,\
             active_time=NULL,active_ns=NULL,active_duration=NULL,previous_time=NULL,\
             previous_ns=NULL WHERE active_owner IS NOT NULL",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1, "one reinstated lease to release");
}

fn admit_checkpoints(database: &Path) {
    rusqlite::Connection::open(database)
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_recovery;")
        .unwrap();
}

#[tokio::test]
async fn quiescent_failed_child_terminal_before_tree_publication_delivers_withheld_result_once() {
    let fixture = Fixture::with_failure_kind(
        "child-terminal-before-tree-publication",
        ModelErrorKind::Protocol,
    );
    let id = fixture.spawn();
    let sessions = fixture.sessions();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("new child session required")
    };
    let source = session.locator();
    let submission = session
        .agent()
        .start(
            CommandId::new("child-crash-terminal").unwrap(),
            "synthetic work",
        )
        .await
        .unwrap();
    let handle = submission.into_result().unwrap();
    fixture.bind_turn(&id, &source, handle.turn_id());
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.submit_result(
                &id,
                &crate::agent_tree_protocol::SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .map_err(|error| error.message)?;
            assert!(
                tree.pending_result_mail("root").is_empty(),
                "result remains withheld before terminal publication"
            );
            Ok(())
        })
        .unwrap();
    let terminal = handle.result().await.unwrap();
    assert!(matches!(terminal.outcome(), TurnResultOutcome::Failed(_)));
    let head = session.release().await.unwrap();
    assert!(sessions.head_is_quiescent(head).await.unwrap());
    assert_ne!(head, source);
    let dispatches = fixture.dispatches();
    // Crash cut: only runtime terminal authority is committed. The tree still
    // binds its pre-turn source and has not published completion or result mail.
    let reopened = ProductAgentTree::reopen(fixture.store.load(&fixture.tree_id).unwrap().unwrap());
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            *tree = reopened;
            Ok(())
        })
        .unwrap();
    assert_eq!(fixture.status(&id), NodeStatus::Paused);
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.resume_tree().map_err(|error| error.message)
        })
        .unwrap();
    crate::agent::rejoin_inflight_after_resume(&fixture.tree_id).await;
    assert_eq!(fixture.attempt_status(&id), AttemptStatus::Failed);
    assert_eq!(fixture.failure_kind(&id).as_deref(), Some("turn_failed"));
    assert_eq!(
        fixture
            .runtime
            .read(|tree| tree.attempt(&id).unwrap().output_json.clone())
            .unwrap()
            .as_deref(),
        Some(r#"{"ok":true}"#)
    );
    assert_eq!(
        fixture
            .runtime
            .read(|tree| tree.pending_result_mail("root").len())
            .unwrap(),
        1
    );
    let message = fixture
        .runtime
        .read(|tree| tree.pending_result_mail("root")[0].message_id.clone())
        .unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.acknowledge_mail(&message)
                .map_err(|error| error.message)
        })
        .unwrap();
    let (ok, repeated) = fixture.recover_agent(&id).await;
    assert!(ok, "{repeated}");
    assert_eq!(repeated["recovered"], false);
    assert!(
        fixture
            .runtime
            .read(|tree| tree.pending_result_mail("root").is_empty())
            .unwrap()
    );
    assert_eq!(
        fixture.dispatches(),
        dispatches,
        "reopen must never redispatch"
    );
}

/// A child stranded by a refused recycle is settled by `recover_agent`
/// through the product reopen: a refusal while the store still refuses says
/// why with its exact code and settles nothing; the retry settles the attempt
/// as an abandoned turn without replaying the model; a followup is admitted
/// again; and close frees the node's slot.
#[tokio::test]
async fn recover_agent_settles_a_stranded_child_through_the_product_reopen() {
    let fixture = Fixture::new("recover-tool");
    let id = fixture.stuck_child().await;
    let dispatched = fixture.dispatches();

    refuse_checkpoints(&fixture.database());
    let (succeeded, refused) = fixture.recover_agent(&id).await;
    assert!(!succeeded, "{refused}");
    assert_eq!(code(&refused), "recovery_storage_unavailable", "{refused}");
    assert_eq!(fixture.status(&id), NodeStatus::RecoveryRequired);
    assert_eq!(
        fixture.failure_kind(&id).as_deref(),
        Some("turn_recovery_required")
    );
    admit_checkpoints(&fixture.database());

    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    assert_eq!(receipt["recovered"], true);
    assert_eq!(receipt["failure_kind"], TURN_ABANDONED);
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    assert_eq!(fixture.attempt_status(&id), AttemptStatus::Failed);
    assert_eq!(fixture.dispatches(), dispatched, "nothing was replayed");

    // The first settlement wins: a repeat reopens nothing and changes nothing.
    let revision = fixture.runtime.read(|tree| tree.revision).unwrap();
    let (succeeded, repeat) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{repeat}");
    assert_eq!(repeat["recovered"], false);
    assert_eq!(
        fixture.runtime.read(|tree| tree.revision).unwrap(),
        revision
    );

    fixture.admit_followup(&id);
    assert_eq!(fixture.attempt_status(&id), AttemptStatus::Running);
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_turn_completed(&id)
                .map_err(|error| error.message)
        })
        .unwrap();
    let before = fixture.live_nodes();
    let (succeeded, closed) = fixture
        .invoke("close_agent", json!({ "agent_id": id }))
        .await;
    assert!(succeeded, "{closed}");
    assert_eq!(fixture.status(&id), NodeStatus::Closed);
    assert_eq!(fixture.live_nodes() + 1, before, "close freed the slot");
}

/// The same stranded child is settled by the background pass a tree open
/// runs, through the same product reopen, and the pass reports it.
#[tokio::test]
async fn the_recovery_pass_settles_a_stranded_child_through_the_product_reopen() {
    let fixture = Fixture::new("recover-pass");
    let quiet: Vec<String> = (0..2).map(|_| fixture.spawn()).collect();
    let id = fixture.stuck_child().await;
    let dispatched = fixture.dispatches();
    let before: Vec<_> = quiet
        .iter()
        .map(|quiet| (fixture.status(quiet), fixture.attempt_status(quiet)))
        .collect();

    let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert_eq!(report.recovered, vec![id.clone()]);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let note = report.note().unwrap();
    assert!(note.contains(&id) && note.contains("close_agent"), "{note}");
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    assert_eq!(fixture.failure_kind(&id).as_deref(), Some(TURN_ABANDONED));
    assert_eq!(fixture.dispatches(), dispatched, "nothing was replayed");
    let after: Vec<_> = quiet
        .iter()
        .map(|quiet| (fixture.status(quiet), fixture.attempt_status(quiet)))
        .collect();
    assert_eq!(before, after, "only the stranded child changed");

    let again = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert!(again.note().is_none(), "a second pass has nothing to do");
    assert!(
        run_tree_recovery_pass("recover-no-such-tree")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn recovery_receipt_describes_the_completed_seal() {
    let fixture = Fixture::new("recover-seal-receipt");
    let id = fixture.stuck_fork().await;
    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    assert_eq!(receipt["status"], "failed");
    assert_eq!(receipt["attempt"], "failed");
    assert_eq!(receipt["failure_kind"], TURN_ABANDONED);
    assert!(receipt.get("workspace_recovery").is_none(), "{receipt}");
    assert!(
        fixture
            .runtime
            .read(|tree| tree.attempt(&id).unwrap().candidate_id.is_some())
            .unwrap()
    );
}

#[tokio::test]
async fn recovery_pass_reports_a_completed_seal_without_ambiguity() {
    let fixture = Fixture::new("recover-seal-pass");
    let id = fixture.stuck_fork().await;
    let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert_eq!(report.recovered, vec![id.clone()]);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert!(!report.note().unwrap().contains("recovery_vcs_ambiguous"));
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
}

#[tokio::test]
async fn recovery_pass_publication_refusal_names_explicit_retry() {
    let fixture = Fixture::new("recover-pass-publish-guidance");
    let id = fixture.stuck_fork().await;
    fixture.store.refuse_sealed.store(true, Ordering::SeqCst);
    let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert_eq!(
        report.refused[0].1.code,
        TopologyErrorCode::RecoveryStorageUnavailable
    );
    let note = report.note().unwrap();
    assert!(note.contains("recover_agent retries"), "{note}");
    assert!(
        note.contains("/recover only selects unresolved child sessions"),
        "{note}"
    );
    assert!(
        !note.contains("/recover (or recover_agent) retries"),
        "{note}"
    );
    fixture.store.refuse_sealed.store(false, Ordering::SeqCst);
    let again = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert!(
        again.note().is_none(),
        "already-settled children are not session recovery targets"
    );
    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    let reopened = ProductAgentTree::reopen(fixture.store.load(&fixture.tree_id).unwrap().unwrap());
    assert_eq!(reopened.node(&id).unwrap().status, NodeStatus::Failed);
    assert_eq!(
        fixture
            .vcs
            .calls_of(crate::occupancy_vcs::fake::FakeOp::Seal)
            .len(),
        1
    );
}

#[tokio::test]
async fn recovery_seal_failure_diagnostic_survives_tree_reopen() {
    let fixture = Fixture::new("recover-seal-failure-reopen");
    let id = fixture.stuck_fork().await;
    fixture
        .vcs
        .seal_next_with(Err(crate::occupancy_vcs::VcsError::seal_failed(
            "persisted seal diagnostic",
        )));
    let (succeeded, refused) = fixture.recover_agent(&id).await;
    assert!(!succeeded);
    assert_eq!(code(&refused), "mint_unavailable");
    let reopened = ProductAgentTree::reopen(fixture.store.load(&fixture.tree_id).unwrap().unwrap());
    let RecoveryNeed::Workspace { code, reason } = reopened.recovery_need(&id).unwrap() else {
        panic!("expected a recorded seal failure");
    };
    assert_eq!(code, TopologyErrorCode::MintUnavailable);
    assert!(reason.contains("persisted seal diagnostic"), "{reason}");
    assert!(
        reason.contains("inspect the copy") && reason.contains("close_agent"),
        "{reason}"
    );
    assert!(
        !reason.contains("whether its snapshot landed is not recorded"),
        "{reason}"
    );
}

#[tokio::test]
async fn recovery_storage_refusal_includes_the_seal_failure() {
    let fixture = Fixture::new("recover-seal-and-storage-failure");
    let id = fixture.stuck_fork().await;
    fixture
        .store
        .refuse_seal_failed
        .store(true, Ordering::SeqCst);
    fixture
        .vcs
        .seal_next_with(Err(crate::occupancy_vcs::VcsError::seal_failed(format!(
            "injected seal failure: {}",
            "s".repeat(1024)
        ))));
    let (succeeded, refused) = fixture.recover_agent(&id).await;
    assert!(!succeeded);
    assert_eq!(code(&refused), "recovery_storage_unavailable");
    let message = refused["message"].as_str().unwrap();
    assert!(message.contains("injected tree store refusal"), "{message}");
    assert!(message.contains("injected seal failure"), "{message}");
    assert!(message.len() <= 512);
    assert_eq!(fixture.failure_kind(&id).as_deref(), Some("seal_failed"));
}

#[tokio::test]
async fn recovery_session_settlement_is_durable_when_a_later_step_fails() {
    for seal_fails in [false, true] {
        let fixture = Fixture::new(if seal_fails {
            "recover-settle-seal-failure"
        } else {
            "recover-settle-publish-failure"
        });
        let id = fixture.stuck_fork().await;
        if seal_fails {
            fixture
                .vcs
                .seal_next_with(Err(crate::occupancy_vcs::VcsError::seal_failed(
                    "seal refused after session publication",
                )));
        } else {
            fixture.store.refuse_sealed.store(true, Ordering::SeqCst);
        }
        let (succeeded, refused) = fixture.recover_agent(&id).await;
        assert!(!succeeded, "{refused}");
        let stored =
            ProductAgentTree::reopen(fixture.store.load(&fixture.tree_id).unwrap().unwrap());
        // The settlement published first; a refused seal then records its
        // own failure over it.
        assert_eq!(
            stored.attempt(&id).unwrap().failure.as_ref().unwrap().kind,
            if seal_fails {
                "seal_failed"
            } else {
                "turn_abandoned"
            },
            "the session settlement is durable even though a later step failed"
        );
    }
}

/// The reopen that abandons a turn moves the head, so a retry's reopen
/// finds it quiescent. With no in-process memory of the abandonment, the
/// retry settles `turn_abandoned` from the child's feed (spec 03 section
/// 14.2), never `recovered_without_turn`.
#[tokio::test]
async fn a_retry_after_a_refused_settlement_derives_the_abandonment_from_the_feed() {
    let fixture = Fixture::new("recover-retry-from-feed");
    let id = fixture.stuck_fork().await;
    fixture.store.refuse.store(true, Ordering::SeqCst);
    let (succeeded, refused) = fixture.recover_agent(&id).await;
    assert!(!succeeded, "{refused}");
    assert_eq!(code(&refused), "recovery_storage_unavailable");
    fixture.store.refuse.store(false, Ordering::SeqCst);
    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    assert_eq!(receipt["recovered"], true, "{receipt}");
    assert_eq!(fixture.failure_kind(&id).as_deref(), Some("turn_abandoned"));
    let stored = ProductAgentTree::reopen(fixture.store.load(&fixture.tree_id).unwrap().unwrap());
    let failure = stored.attempt(&id).unwrap().failure.clone().unwrap();
    assert!(
        !failure.message.contains("no abandonment evidence"),
        "{failure:?}"
    );
}

#[tokio::test]
async fn recovery_seal_failure_is_reported_not_as_vcs_ambiguity() {
    for pass in [false, true] {
        let fixture = Fixture::new(if pass {
            "recover-seal-fail-pass"
        } else {
            "recover-seal-fail-tool"
        });
        let id = fixture.stuck_fork().await;
        fixture
            .vcs
            .seal_next_with(Err(crate::occupancy_vcs::VcsError::seal_failed(
                "injected seal failure",
            )));
        if pass {
            let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
            assert_eq!(report.recovered, [] as [String; 0]);
            assert_eq!(report.refused.len(), 1);
            assert_eq!(report.refused[0].1.code, TopologyErrorCode::MintUnavailable);
            assert!(
                report.refused[0]
                    .1
                    .message
                    .contains("injected seal failure")
            );
        } else {
            let (succeeded, refused) = fixture.recover_agent(&id).await;
            assert!(!succeeded, "{refused}");
            assert_eq!(code(&refused), "mint_unavailable", "{refused}");
            assert!(
                refused["message"]
                    .as_str()
                    .unwrap()
                    .contains("injected seal failure")
            );
        }
        assert_eq!(fixture.status(&id), NodeStatus::RecoveryRequired);
        assert_eq!(fixture.failure_kind(&id).as_deref(), Some("seal_failed"));
        let (succeeded, refused) = fixture.recover_agent(&id).await;
        assert!(!succeeded);
        assert_eq!(code(&refused), "mint_unavailable", "{refused}");
    }
}

#[tokio::test]
async fn recovery_seal_publication_failure_is_reported_as_storage_unavailable() {
    for pass in [false, true] {
        let fixture = Fixture::new(if pass {
            "recover-seal-publish-pass"
        } else {
            "recover-seal-publish-tool"
        });
        let id = fixture.stuck_fork().await;
        fixture.store.refuse_sealed.store(true, Ordering::SeqCst);
        if pass {
            let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
            assert_eq!(report.recovered, [] as [String; 0]);
            assert_eq!(report.refused.len(), 1);
            assert_eq!(
                report.refused[0].1.code,
                TopologyErrorCode::RecoveryStorageUnavailable
            );
        } else {
            let (succeeded, refused) = fixture.recover_agent(&id).await;
            assert!(!succeeded, "{refused}");
            assert_eq!(code(&refused), "recovery_storage_unavailable", "{refused}");
        }
        assert!(
            fixture
                .runtime
                .read(|tree| tree.attempt(&id).unwrap().candidate_id.is_some())
                .unwrap(),
            "the seal completion was recorded even though its publish failed"
        );
        let persisted = fixture.store.load(&fixture.tree_id).unwrap().unwrap();
        let reopened = ProductAgentTree::reopen(persisted);
        assert_eq!(
            reopened.node(&id).unwrap().status,
            NodeStatus::RecoveryRequired,
            "the durable seal remains unresolved"
        );
        let (succeeded, refused) = fixture.recover_agent(&id).await;
        assert!(
            !succeeded,
            "an in-memory seal must not hide a refused publish: {refused}"
        );
        assert_eq!(code(&refused), "recovery_storage_unavailable");
        fixture.store.refuse_sealed.store(false, Ordering::SeqCst);
        let (succeeded, receipt) = fixture.recover_agent(&id).await;
        assert!(succeeded, "{receipt}");
        assert_eq!(receipt["status"], "failed");
        assert_eq!(
            receipt["recovered"], false,
            "the session was already settled"
        );
        let reopened =
            ProductAgentTree::reopen(fixture.store.load(&fixture.tree_id).unwrap().unwrap());
        assert_eq!(reopened.node(&id).unwrap().status, NodeStatus::Failed);
        assert_eq!(
            fixture
                .vcs
                .calls_of(crate::occupancy_vcs::fake::FakeOp::Seal)
                .len(),
            1,
            "retry publishes without resealing"
        );
    }
}

/// The pass deadline covers waiting for the reattach lock: a pass that
/// cannot get it in time reports every target as not attempted and touches
/// nothing.
#[tokio::test]
async fn a_pass_that_cannot_get_the_lock_by_its_deadline_reports_not_attempted() {
    let fixture = Fixture::new("recover-pass-deadline");
    let id = fixture.stuck_child().await;
    let before = fixture.snapshot(&id);
    let lock = fixture.runtime.lock_reattach().await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
    let report = run_tree_recovery_pass_by(&fixture.tree_id, deadline)
        .await
        .unwrap();
    drop(lock);
    assert!(report.recovered.is_empty(), "{report:?}");
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(report.not_attempted, vec![id.clone()]);
    assert!(report.note().unwrap().contains(&id));
    let after = fixture.snapshot(&id);
    assert_eq!(
        (after.1.status, after.2),
        (before.1.status, before.2),
        "nothing was settled"
    );
}

/// A lease another owner still holds is `recovery_lease_held`, exactly:
/// `recover_agent` waits out only its own short wait, and the background
/// pass never waits. Once the holder lets go, the retry settles the attempt.
#[tokio::test]
async fn a_held_lease_is_refused_as_recovery_lease_held() {
    let fixture = Fixture::new("recover-lease");
    let id = fixture.spawn();
    // The holder is an attachment this process still has open: the store's
    // dead-holder probe cannot reclaim it, so the lease stays held.
    let holder = fixture.sessions();
    let (held, handle, _) = fixture.start_failing_turn(&holder, &id).await;
    Fixture::await_unknown_outcome(&handle).await;
    fixture.bind_turn(&id, &held.locator(), handle.turn_id());
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_turn_failure(
                &id,
                crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                "child turn outcome is unknown".into(),
            )
            .map_err(|error| error.message)
        })
        .unwrap();
    let before = fixture.snapshot(&id);

    // The background pass makes exactly the try `recover_agent` repeats,
    // without waiting; its duration is one try's cost on this host now.
    let pass_started = std::time::Instant::now();
    let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    let one_try = pass_started.elapsed();
    assert_eq!(report.recovered, [] as [String; 0]);
    assert_eq!(report.refused.len(), 1);
    assert_eq!(report.refused[0].0, id);
    assert_eq!(
        report.refused[0].1.code,
        TopologyErrorCode::RecoveryLeaseHeld
    );
    assert!(report.note().unwrap().contains("recovery_lease_held"));

    // The lease wait outlasts several tries however loaded the host is, so
    // there is a poll between tries for the probe to observe.
    let lease_wait = Duration::from_millis(1500).max(one_try * 3);
    test_lease_wait::set(lease_wait);
    let started = std::time::Instant::now();
    // Between polls of the held lease the reattach lock is free, so the
    // tree's other recoveries and reattaches are not queued behind it. The
    // probe is budgeted by the lease wait, not by wall time: it must see the
    // lock taken and then free again before `recover_agent` returns.
    let recover = fixture.recover_agent(&id);
    tokio::pin!(recover);
    let mut seen_held = false;
    let mut freed_while_waiting = false;
    let (succeeded, refused) = loop {
        tokio::select! {
            biased;
            outcome = &mut recover => break outcome,
            () = tokio::time::sleep(Duration::from_millis(5)) => {
                match fixture.runtime.try_lock_reattach() {
                    Err(_) => seen_held = true,
                    Ok(_) => freed_while_waiting |= seen_held,
                }
            }
        }
    };
    assert!(
        freed_while_waiting,
        "recover_agent held the reattach lock while polling"
    );
    assert!(!succeeded, "{refused}");
    assert_eq!(code(&refused), "recovery_lease_held", "{refused}");
    assert!(started.elapsed() >= lease_wait, "it waited");
    assert!(started.elapsed() < lease_wait + Duration::from_secs(20));
    assert_eq!(fixture.snapshot(&id).1.status, before.1.status);
    assert_eq!(
        fixture.failure_kind(&id).as_deref(),
        Some("turn_recovery_required")
    );

    // The holder let go by an ordinary release, which closed the turn with
    // an unknown outcome as abandoned. That closure is in the child's feed,
    // so the retry settles the attempt from it (spec 03 section 14.2),
    // although the head it reopens is quiescent.
    held.release().await.unwrap();
    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    assert_eq!(receipt["recovered"], true);
    assert_eq!(receipt["failure_kind"], "turn_abandoned");
}

/// A refused tree publish leaves the tree exactly as it was and is reported
/// as `recovery_storage_unavailable`; the retry still settles the attempt as
/// the abandoned turn the first reopen observed, not as a quiescent head.
#[tokio::test]
async fn a_refused_publish_changes_nothing_and_the_retry_still_reports_the_abandonment() {
    let fixture = Fixture::new("recover-publish");
    let id = fixture.stuck_child().await;
    let before = fixture.snapshot(&id);
    fixture.store.refuse.store(true, Ordering::SeqCst);
    let (succeeded, refused) = fixture.recover_agent(&id).await;
    assert!(!succeeded, "{refused}");
    assert_eq!(code(&refused), "recovery_storage_unavailable", "{refused}");
    let after = fixture.snapshot(&id);
    assert_eq!(before.1.status, after.1.status);
    assert_eq!(before.2, after.2);
    assert_eq!(
        fixture.failure_kind(&id).as_deref(),
        Some("turn_recovery_required")
    );
    fixture.store.refuse.store(false, Ordering::SeqCst);

    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    assert_eq!(receipt["failure_kind"], TURN_ABANDONED);
}

/// A recovery and the turn waiter's own late failure report settle one
/// attempt once: while the waiter holds the reattach lock, `recover_agent`
/// waits; after the waiter settled, the recovery is a no-op; and a waiter
/// report that arrives after a recovery changes nothing.
#[tokio::test]
async fn a_recovery_and_the_waiter_settle_an_attempt_once() {
    let fixture = Fixture::new("recover-interleave");
    let id = fixture.stuck_child().await;
    let attempt_id = fixture
        .runtime
        .read(|tree| tree.attempt(&id).unwrap().attempt_id.clone())
        .unwrap();

    // The waiter is mid-recycle: it holds the lock until it has settled.
    let lock = fixture.runtime.lock_reattach().await;
    let recovering = {
        let tree_id = fixture.tree_id.clone();
        let id = id.clone();
        tokio::spawn(async move {
            crate::agent::invoke_topology_tool_on_hub(
                &tree_id,
                None,
                "recover_agent",
                &json!({ "agent_id": id }),
                "recover:interleave",
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !recovering.is_finished(),
        "recover_agent waits for the lock"
    );
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.recover_host_decision(
                &id,
                &attempt_id,
                RecoveryResolution::TurnAbandoned {
                    message: "the waiter's recycle abandoned it".into(),
                },
            )
            .map(|_| ())
            .map_err(|error| error.message)
        })
        .unwrap();
    drop(lock);
    let receipt = recovering.await.unwrap().unwrap();
    assert!(receipt.succeeded, "{}", receipt.detail);
    let receipt: serde_json::Value = serde_json::from_str(&receipt.detail).unwrap();
    assert_eq!(receipt["recovered"], false);

    let revision = fixture.runtime.read(|tree| tree.revision).unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_turn_failure(
                &id,
                crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                "late waiter".into(),
            )
            .map_err(|error| error.message)
        })
        .unwrap();
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    assert_eq!(fixture.failure_kind(&id).as_deref(), Some(TURN_ABANDONED));
    assert_eq!(
        fixture.runtime.read(|tree| tree.revision).unwrap(),
        revision
    );
}

async fn interrupted_adopt(fixture: &Fixture) -> String {
    let id = fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "implement", "work")
                    .with_fork(),
                None,
            )
            .map(|spawned| spawned.agent_id)
            .map_err(|error| error.message)
        })
        .unwrap();
    let sessions = fixture.sessions();
    let (session, handle, _) = fixture.start_failing_turn(&sessions, &id).await;
    Fixture::await_unknown_outcome(&handle).await;
    fixture.bind_turn(&id, &session.locator(), handle.turn_id());
    session.release().await.unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_turn_completed(&id)
                .map_err(|error| error.message)
        })
        .unwrap();
    let candidate = fixture
        .runtime
        .read(|tree| tree.attempt(&id).unwrap().candidate_id.clone())
        .unwrap()
        .expect("the completed attempt was sealed");
    // The adopt starts and the process is lost before it lands: its effects
    // are never run, and the retirement is left unresolved.
    let ((), _abandoned) = fixture
        .runtime
        .driver()
        .mutate(|tree| {
            tree.begin_adopt(
                "root",
                &AdoptCandidateRequest {
                    candidate_id: candidate,
                    expected_target: None,
                    evidence: None,
                },
            )
            .unwrap();
            tree.record_host_retirement(&id, "the process was lost mid-adopt".into())
                .and_then(|value| tree.fold_due(&id).map(|_| value))
                .unwrap();
            tree.acknowledge_revision(fixture.store.save(tree).unwrap());
            *tree =
                ProductAgentTree::reopen(fixture.store.load(&fixture.tree_id).unwrap().unwrap());
            tree.set_vcs(fixture.vcs.clone());
        })
        .unwrap();

    id
}

#[tokio::test]
async fn recovery_pass_keeps_a_genuinely_interrupted_adopt_ambiguous() {
    let fixture = Fixture::new("recover-adopt-pass");
    let id = interrupted_adopt(&fixture).await;
    let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert_eq!(report.recovered, [] as [String; 0]);
    assert_eq!(report.refused.len(), 1);
    assert_eq!(report.refused[0].0, id);
    assert_eq!(
        report.refused[0].1.code,
        TopologyErrorCode::RecoveryVcsAmbiguous
    );
    assert_eq!(fixture.status(&id), NodeStatus::RecoveryRequired);
}

/// An interrupted adopt behind a session failure: `recover_agent` settles the
/// session part and says what remains; a repeat is `recovery_vcs_ambiguous`;
/// `close_agent` ends the node and frees its slot.
#[tokio::test]
async fn an_interrupted_adopt_settles_its_session_then_closes() {
    let fixture = Fixture::new("recover-adopt");
    let id = interrupted_adopt(&fixture).await;

    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    assert_eq!(receipt["recovered"], true);
    assert_eq!(receipt["status"], "recovery_required");
    assert!(
        receipt["workspace_recovery"]
            .as_str()
            .is_some_and(|reason| reason.contains("close_agent")),
        "{receipt}"
    );
    let (succeeded, refused) = fixture.recover_agent(&id).await;
    assert!(!succeeded);
    assert_eq!(code(&refused), "recovery_vcs_ambiguous", "{refused}");

    let before = fixture.live_nodes();
    let (succeeded, closed) = fixture
        .invoke("close_agent", json!({ "agent_id": id }))
        .await;
    assert!(succeeded, "{closed}");
    assert_eq!(fixture.status(&id), NodeStatus::Closed);
    assert_eq!(fixture.live_nodes() + 1, before);
}

/// A leaf child may not recover its sibling: the tool is not compiled for
/// it, so the refusal comes before any reopen. (A spawner that does not
/// manage the node is `grant_denied`; the tree's unit tests pin that.)
#[tokio::test]
async fn recover_agent_is_refused_to_a_caller_that_does_not_manage_the_node() {
    let fixture = Fixture::new("recover-authority");
    let id = fixture.stuck_child().await;
    let sibling = fixture.spawn();
    let refused = crate::agent::invoke_topology_tool_on_hub(
        &fixture.tree_id,
        Some(&sibling),
        "recover_agent",
        &json!({ "agent_id": id }),
        "recover:sibling",
    )
    .await;
    let Err(refused) = refused else {
        panic!("a leaf caller was granted recover_agent")
    };
    assert_eq!(
        refused,
        format!("topology tool `recover_agent` is not granted to `{sibling}`")
    );
    assert_eq!(fixture.status(&id), NodeStatus::RecoveryRequired);
}

/// Installs a child binding agent construction refuses before it writes
/// anything, the way the A2 `read-execute` reviewer's unadmitted projection
/// was refused.
fn install_construction_refused_binding(
    fixture: &Fixture,
) -> crate::agent::injected_child_binding::Guard {
    let workspace = fixture.root.path().to_path_buf();
    let evidence = fixture.evidence.clone();
    crate::agent::injected_child_binding::install(Arc::new(move |_, _, _| {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                .map_err(|error| error.to_string())?;
        plakous_offline_crash_harness::construction_refused_runtime_binding(&workspace, controller)
            .map_err(|error| error.to_string())
    }))
}

/// A spawn whose create construction refused created nothing, so the spawn
/// is a definite refusal (the node fails as `spawn_refused`), not a startup
/// left for recovery that no replay can ever settle.
#[tokio::test]
async fn a_create_refused_before_it_wrote_anything_is_a_definite_spawn_refusal() {
    let fixture = Fixture::new("spawn-create-refused");
    let _refused = install_construction_refused_binding(&fixture);
    let spawned = fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map_err(|error| error.message)
        })
        .unwrap();
    let operation = spawn_child_operation_id(&fixture.tree_id, &spawned.agent_id).unwrap();
    let start = crate::agent_tree_tools::SpawnStart {
        path: fixture.root.path().display().to_string(),
        spawned,
        description: "review".into(),
        prompt: "review the diff".into(),
        output_schema_json: "{}".into(),
        turn_token: 1,
        tier: Tier::ReadOnly,
        account: "claude".into(),
        tuple_id: "claude-opus-5-5".into(),
        effort: None,
    };
    let created =
        crate::agent::children::create_spawned_child_session(&fixture.parent, &start).await;
    let message = match created {
        Err(crate::agent_tree_tools::SpawnBindError::Refused(message)) => message,
        Err(crate::agent_tree_tools::SpawnBindError::RecoveryRequired { message, .. }) => {
            panic!("a create that wrote nothing was left for recovery: {message}")
        }
        Ok(_) => panic!("a construction-refused binding created a session"),
    };
    assert!(
        message.contains("child session could not be created (Revalidation)"),
        "{message}"
    );
    assert_eq!(
        fixture
            .sessions()
            .accepted_create(&operation)
            .await
            .unwrap(),
        None
    );
}

/// A startup whose create never produced a session (no locator, no session
/// for the spawn's deterministic create, and the replayed create refused
/// again) has nothing to reopen. Recovery settles it `failed` as
/// `spawn_refused` instead of refusing `recovery_session_unavailable`
/// forever; the node is then idle, `wait_agent` answers, and `close_agent`
/// ends it.
#[tokio::test]
async fn a_startup_whose_create_never_produced_a_session_settles_failed_and_closes() {
    let fixture = Fixture::new("recover-never-created");
    let _refused = install_construction_refused_binding(&fixture);
    let id = fixture.spawn();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_host_retirement(
                &id,
                "startup Creating: child session could not be created (Revalidation)".into(),
            )
            .and_then(|value| tree.fold_due(&id).map(|_| value))
            .map_err(|error| error.message)
        })
        .unwrap();
    assert_eq!(fixture.status(&id), NodeStatus::RecoveryRequired);
    assert!(fixture.locator(&id).is_none());

    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    assert_eq!(receipt["recovered"], json!(true), "{receipt}");
    assert_eq!(fixture.failure_kind(&id).as_deref(), Some("spawn_refused"));
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    assert_eq!(fixture.attempt_status(&id), AttemptStatus::Failed);
    assert!(fixture.locator(&id).is_none(), "no session was bound");
    let operation = spawn_child_operation_id(&fixture.tree_id, &id).unwrap();
    assert_eq!(
        fixture
            .sessions()
            .accepted_create(&operation)
            .await
            .unwrap(),
        None
    );

    let (waited, receipt) = fixture
        .invoke("wait_agent", json!({ "agent_ids": [id], "timeout_ms": 1 }))
        .await;
    assert!(waited, "{receipt}");
    let (closed, receipt) = fixture
        .invoke("close_agent", json!({ "agent_id": id }))
        .await;
    assert!(closed, "{receipt}");
    assert_eq!(fixture.status(&id), NodeStatus::Closed);
    assert_eq!(fixture.dispatches(), 0);
}

/// A node recovery settled `spawn_refused` never produced a session, so a
/// `followup` has nothing to run in: it is refused at admission with
/// `followup_closed` (not admitted and then failed closed at delivery, which
/// would consume the brief and put the node back to `recovery_required`). The
/// node stays failed and closable.
#[tokio::test]
async fn followup_to_a_never_created_child_is_refused_and_the_node_stays_closable() {
    let fixture = Fixture::new("followup-never-created");
    let refused = install_construction_refused_binding(&fixture);
    let id = fixture.spawn();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_host_retirement(
                &id,
                "startup Creating: child session could not be created (Revalidation)".into(),
            )
            .and_then(|value| tree.fold_due(&id).map(|_| value))
            .map_err(|error| error.message)
        })
        .unwrap();
    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    assert_eq!(fixture.failure_kind(&id).as_deref(), Some("spawn_refused"));
    assert_eq!(fixture.status(&id), NodeStatus::Failed);

    // Even with a working binding (the refusal is gone), the node has no
    // session to re-task.
    drop(refused);
    let workspace = fixture.root.path().to_path_buf();
    let evidence = fixture.evidence.clone();
    let _working = crate::agent::injected_child_binding::install(Arc::new(move |_, _, _| {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                .map_err(|error| error.to_string())?;
        projected_terminal_failure_runtime_binding(
            &workspace,
            controller,
            ModelErrorKind::OutcomeUnknown,
        )
        .map_err(|error| error.to_string())
    }));

    let (sent, receipt) = fixture
        .invoke(
            "send_agent_message",
            json!({ "agent_id": id, "message": "try again", "purpose": "followup" }),
        )
        .await;
    assert!(
        !sent,
        "a never-created child cannot be re-tasked: {receipt}"
    );
    assert_eq!(code(&receipt), "followup_closed", "{receipt}");
    assert!(
        receipt["message"]
            .as_str()
            .is_some_and(|message| message.contains("never produced a session")),
        "{receipt}"
    );
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    assert_eq!(fixture.attempt_status(&id), AttemptStatus::Failed);
    assert_eq!(fixture.failure_kind(&id).as_deref(), Some("spawn_refused"));
    assert!(fixture.locator(&id).is_none());

    let (closed, receipt) = fixture
        .invoke("close_agent", json!({ "agent_id": id }))
        .await;
    assert!(closed, "{receipt}");
    assert_eq!(fixture.status(&id), NodeStatus::Closed);
    assert_eq!(fixture.dispatches(), 0);
}

/// A `followup` whose Start finds the child's previous slot still retiring
/// is retried through the product deliverer once the slot is vacant: the new
/// attempt's turn is admitted and dispatched. It is never left unread under
/// a `started` receipt on an attempt nothing will start.
#[tokio::test]
async fn a_followup_while_the_previous_slot_retires_starts_once_it_is_vacant() {
    let fixture = Fixture::new("followup-retiring");
    // A native account, so the product composes the followup's Start.
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.pin_bind_hints_for_test(crate::orchestration::BindHints::from_names(
                &["claude"],
                &[],
            ));
            tree.pin_template(
                "review",
                crate::orchestration::RosterBinding::leaf("claude", "test-model"),
            );
            Ok(())
        })
        .unwrap();
    let id = fixture.stuck_child().await;
    let (recovered, receipt) = fixture.recover_agent(&id).await;
    assert!(recovered, "{receipt}");
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    let dispatched = fixture.dispatches();
    // The previous turn's live slot is still being released.
    let OpenSessionOutcome::Opened { session, .. } = fixture
        .sessions()
        .open(OpenSessionRequest::new(
            SessionOperationId::new("followup-retiring:previous").unwrap(),
            fixture.locator(&id).unwrap().parse().unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("the failed child's session reopens");
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        fixture
            .runtime
            .try_occupy_live(
                id.clone(),
                Box::new(LateReleaseChild {
                    sessions: fixture.sessions(),
                    session,
                    turn: "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    let retiring = tokio::spawn({
        let (runtime, id) = (fixture.runtime.clone(), id.clone());
        async move {
            crate::topology_runtime::retire_live(runtime.driver(), &runtime.live, &id, None, false)
                .await
        }
    });
    entered.notified().await;
    let sent = tokio::spawn({
        let (tree_id, id) = (fixture.tree_id.clone(), id.clone());
        async move {
            crate::agent::invoke_topology_tool_on_hub(
                &tree_id,
                None,
                "send_agent_message",
                &json!({ "agent_id": id, "message": "try again", "purpose": "followup" }),
                "followup-retiring",
            )
            .await
            .unwrap()
        }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while fixture.attempt_status(&id) != AttemptStatus::Running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the followup is admitted");
    // Its first Start finds the slot retiring.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!sent.is_finished(), "the send waits for the slot");
    finish.notify_one();
    retiring.await.unwrap().unwrap();
    let receipt = tokio::time::timeout(Duration::from_secs(30), sent)
        .await
        .expect("the send returns")
        .unwrap();
    assert!(receipt.succeeded, "{}", receipt.detail);
    let sent: serde_json::Value = serde_json::from_str(&receipt.detail).unwrap();
    assert_eq!(sent["disposition"], "started", "{sent}");
    tokio::time::timeout(Duration::from_secs(10), async {
        while fixture.dispatches() == dispatched {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the followup's turn was admitted and dispatched");
}

/// Startup uncertainty: the create may or may not have run, and no locator
/// was published. Replaying the spawn's own create settles both cases.
#[tokio::test]
async fn startup_uncertainty_settles_through_the_spawns_create() {
    for present in [true, false] {
        let fixture = Fixture::new(if present {
            "recover-startup-present"
        } else {
            "recover-startup-absent"
        });
        let id = fixture.spawn();
        let created = if present {
            let CreateSessionOutcome::Created { session, .. } = fixture
                .sessions()
                .create(CreateSessionRequest::new(
                    spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
                    fixture.binding(),
                ))
                .await
                .unwrap()
            else {
                panic!("created session required")
            };
            Some(session.release().await.unwrap())
        } else {
            None
        };
        fixture
            .runtime
            .mutate_then_emit(false, |tree| {
                tree.record_host_retirement(
                    &id,
                    "startup Creating: child session could not be created".into(),
                )
                .and_then(|value| tree.fold_due(&id).map(|_| value))
                .map_err(|error| error.message)
            })
            .unwrap();
        assert!(fixture.locator(&id).is_none());

        let (succeeded, receipt) = fixture.recover_agent(&id).await;
        assert!(succeeded, "{receipt}");
        assert_eq!(
            fixture.failure_kind(&id).as_deref(),
            Some(RECOVERED_WITHOUT_TURN)
        );
        assert_eq!(fixture.status(&id), NodeStatus::Failed);
        assert_ne!(fixture.attempt_status(&id), AttemptStatus::Completed);
        let bound: SessionLocator = fixture.locator(&id).unwrap().parse().unwrap();
        if let Some(created) = created {
            assert_eq!(
                bound.session_id(),
                created.session_id(),
                "the replay found the session the spawn created"
            );
        }
        assert_eq!(fixture.dispatches(), 0);
    }
}

/// Lost locator publication must reconstruct before the real child binder runs.
#[tokio::test]
async fn publication_cut_reconstructs_child_before_binding_and_new_children_use_current_sidecar() {
    use plakous_offline_crash_harness::script::ScriptPlan;
    use plakous_offline_crash_harness::scripted_code_mode_runtime_binding;

    struct Current(PathBuf);
    impl plakous::tools::SidecarLocator for Current {
        fn javascript_host(&self) -> Result<PathBuf, String> {
            Ok(self.0.clone())
        }
        fn python_host(&self) -> Result<PathBuf, String> {
            Err("not a Python fixture".into())
        }
    }
    let mut fixture = Fixture::new("publication-cut-sidecar");
    let historical = fixture.root.path().join("historical-host");
    let current = fixture.root.path().join("current-host");
    std::fs::write(&historical, b"historical worker").unwrap();
    std::fs::write(&current, b"changed current worker").unwrap();
    let historical = historical.canonicalize().unwrap();
    let current = current.canonicalize().unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&observed);
    let workspace = fixture.root.path().to_path_buf();
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &fixture.evidence)
            .unwrap();
    let script = Arc::new(serde_json::from_value::<ScriptPlan>(json!({"version": 1})).unwrap());
    let _binding = crate::agent::injected_child_binding::install(Arc::new(move |_, _, roots| {
        let path = roots.sidecars().javascript_host()?;
        seen.lock().unwrap().push(path.clone());
        scripted_code_mode_runtime_binding(
            &workspace,
            controller.clone(),
            Arc::clone(&script),
            Arc::new(AtomicU64::new(0)),
            &path,
            None,
        )
        .map_err(|error| error.to_string())
    }));
    let SpawnParent::Api(spec) = &mut fixture.parent else {
        panic!("API fixture")
    };
    spec.state_roots = spec
        .state_roots
        .clone()
        .with_test_sidecars(Arc::new(Current(historical.clone())));
    let id = fixture.spawn();
    let operation = spawn_child_operation_id(&fixture.tree_id, &id).unwrap();
    let sessions = fixture.sessions();
    let binding = child_session_runtime_binding(
        &fixture.parent,
        &id,
        fixture.root.path().to_str().unwrap(),
        None,
        None,
        None,
        false,
        super::super::SessionToolFence::Full,
        None,
    )
    .await
    .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(operation.clone(), binding))
        .await
        .unwrap()
    else {
        panic!("created child required")
    };
    let created = session.release().await.unwrap();
    assert!(fixture.locator(&id).is_none(), "publication cut");
    let SpawnParent::Api(spec) = &mut fixture.parent else {
        panic!("API fixture")
    };
    spec.state_roots = spec
        .state_roots
        .clone()
        .with_test_sidecars(Arc::new(Current(current.clone())));
    let port = ParentSessionPort {
        parent: fixture.parent.clone(),
    };
    let ReplayedCreate::Session(recovered) = port
        .replay_create(&fixture.runtime, &id)
        .await
        .unwrap_or_else(|error| panic!("{}", error.error.message))
    else {
        panic!("the replay found no session")
    };
    assert_eq!(recovered, created.to_string());

    // An own-child exact reopen retains its recorded artifact. A genuinely new
    // child of that pinned parent must instead compile against today's sidecar.
    let SpawnParent::Api(spec) = &mut fixture.parent else {
        panic!("API fixture")
    };
    spec.state_roots = spec
        .state_roots
        .clone()
        .for_exact_reopen(&sessions, created)
        .await
        .unwrap();
    let own = child_session_runtime_binding(
        &fixture.parent,
        &id,
        fixture.root.path().to_str().unwrap(),
        None,
        None,
        None,
        true,
        super::super::SessionToolFence::Full,
        Some(
            sessions
                .created_request_output_bound(created)
                .await
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:own-child").unwrap(),
            created,
            own,
        ))
        .await
        .unwrap()
    else {
        panic!("exact own child open")
    };
    assert_eq!(session.release().await.unwrap(), created);
    let new_id = fixture.spawn();
    let ReplayedCreate::Session(new_locator) = ParentSessionPort {
        parent: fixture.parent.clone(),
    }
    .replay_create(&fixture.runtime, &new_id)
    .await
    .unwrap_or_else(|error| panic!("{}", error.error.message)) else {
        panic!("the replay created no session")
    };
    assert_ne!(new_locator, recovered);
    assert_eq!(
        *observed.lock().unwrap(),
        [historical.clone(), historical.clone(), historical, current]
    );
    assert_eq!(fixture.dispatches(), 0);
}

/// A session port that records what it was asked and refuses.
#[derive(Default)]
struct RefusingPort {
    reopens: AtomicUsize,
    creates: AtomicUsize,
    reopen_error: Mutex<Option<TopologyError>>,
    terminal: AtomicBool,
}

impl ChildSessionPort for RefusingPort {
    fn reopen<'a>(&'a self, _: &'a str, _: &'a str, _: &'a str) -> PortFuture<'a, ChildReopened> {
        self.reopens.fetch_add(1, Ordering::SeqCst);
        let error = self.reopen_error.lock().unwrap().clone();
        let terminal = self.terminal.load(Ordering::SeqCst);
        Box::pin(async move {
            Err(PortRefusal {
                error: error.unwrap_or_else(|| session_refusal("the port was asked to reopen")),
                terminal,
            })
        })
    }

    fn replay_create<'a>(
        &'a self,
        _: &'a ProductTopologyRuntime,
        _: &'a str,
    ) -> PortFuture<'a, ReplayedCreate> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(session_refusal("the port was asked to create").into()) })
    }
}

/// A refusal settles nothing and records nothing on the node, whether it
/// comes from `recover_child` or from the pass.
#[tokio::test]
async fn a_refusal_records_nothing_on_the_node() {
    let fixture = Fixture::new("recover-effect");
    let id = fixture.stuck_child().await;
    let port = RefusingPort::default();
    *port.reopen_error.lock().unwrap() = Some(refusal(
        TopologyErrorCode::RecoveryEffectEvidenceRefused,
        "the child's head needs executor effect evidence this host does not hold",
    ));
    let before = fixture.snapshot(&id);
    let error = recover_child(&fixture.runtime, &port, &id, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::RecoveryEffectEvidenceRefused);
    assert_eq!(fixture.snapshot(&id), before);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let report = recover_unresolved_children(&fixture.runtime, &port, deadline).await;
    assert_eq!(report.refused.len(), 1);
    assert_eq!(
        report.refused[0].1.code,
        TopologyErrorCode::RecoveryEffectEvidenceRefused
    );
    assert_eq!(fixture.snapshot(&id), before);
}

/// Every typed reopen refusal maps to its one recovery code.
#[test]
fn reopen_refusals_map_to_exact_codes() {
    for (kind, expected) in [
        (
            ChildReopenErrorKind::LeaseHeld {
                retry_after_seconds: Some(30),
            },
            TopologyErrorCode::RecoveryLeaseHeld,
        ),
        (
            ChildReopenErrorKind::LeaseHeld {
                retry_after_seconds: None,
            },
            TopologyErrorCode::RecoveryLeaseHeld,
        ),
        (
            ChildReopenErrorKind::StorageUnavailable,
            TopologyErrorCode::RecoveryStorageUnavailable,
        ),
        (
            ChildReopenErrorKind::Incompatible,
            TopologyErrorCode::RecoverySessionUnavailable,
        ),
        (
            ChildReopenErrorKind::SessionUnavailable,
            TopologyErrorCode::RecoverySessionUnavailable,
        ),
    ] {
        let error = reopen_refusal(ChildReopenError {
            kind,
            message: "refused".into(),
        });
        assert_eq!(error.code, expected, "{kind:?}");
    }
}

/// A child whose turn this process still owns is not reopened under it.
#[tokio::test]
async fn a_live_owner_is_never_reopened_under() {
    let fixture = Fixture::new("recover-live");
    let id = fixture.spawn();
    // A startup past its caller's deadline: the node reads recovery-required
    // while this process's startup owner still holds the slot.
    let startup = fixture.runtime.reserve_startup(&id).await.unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_host_retirement(&id, "startup Starting: deadline".into())
                .and_then(|value| tree.fold_due(&id).map(|_| value))
                .map_err(|error| error.message)
        })
        .unwrap();
    let port = RefusingPort::default();
    let error = recover_child(&fixture.runtime, &port, &id, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::RecoveryOwnerLive);
    assert_eq!(port.reopens.load(Ordering::SeqCst), 0);
    assert_eq!(port.creates.load(Ordering::SeqCst), 0);
    drop(startup);
}

/// A refusal no retry can get past (here, a head that needs effect evidence
/// the host does not hold) does not strand the node: `list_agents` names
/// `close_agent`, the pass stops retrying it, and closing it settles the
/// attempt as failed with the refusal's code and frees its slot.
#[tokio::test]
async fn a_terminal_refusal_lets_close_agent_end_the_node() {
    let fixture = Fixture::new("recover-terminal");
    let id = fixture.stuck_child().await;
    let port = RefusingPort::default();
    *port.reopen_error.lock().unwrap() = Some(refusal(
        TopologyErrorCode::RecoveryEffectEvidenceRefused,
        "the child's head needs executor effect evidence this host does not hold",
    ));
    port.terminal.store(true, Ordering::SeqCst);
    let error = recover_child(&fixture.runtime, &port, &id, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::RecoveryEffectEvidenceRefused);
    assert_eq!(fixture.status(&id), NodeStatus::RecoveryRequired);
    let (_, rows) = fixture.invoke("list_agents", json!({})).await;
    let row = rows["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["agent_id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["remedy"], "close_agent", "{row}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let report = recover_unresolved_children(&fixture.runtime, &port, deadline).await;
    assert!(report.refused.is_empty() && report.recovered.is_empty());
    assert_eq!(port.reopens.load(Ordering::SeqCst), 1);

    let (closed, receipt) = fixture
        .invoke("close_agent", json!({ "agent_id": id }))
        .await;
    assert!(closed, "{receipt}");
    assert_eq!(fixture.status(&id), NodeStatus::Closed);
    assert_eq!(fixture.attempt_status(&id), AttemptStatus::Failed);
    assert_eq!(
        fixture.failure_kind(&id).as_deref(),
        Some("recovery_effect_evidence_refused")
    );
}

/// A fenced child's turn waiter recycles the session outside the reattach
/// lock: while its reopen waits out a lease another process holds,
/// `recover_agent` and the recovery pass answer `recovery_owner_live` at
/// once instead of queueing behind it.
#[tokio::test]
async fn a_recycling_waiter_is_an_owner_that_recovery_never_waits_on() {
    let fixture = Fixture::new("recover-recycling");
    let id = fixture.spawn();
    let holder = fixture.sessions();
    let (held, handle, control) = fixture.start_failing_turn(&holder, &id).await;
    let turn = handle.turn_id();
    Fixture::await_unknown_outcome(&handle).await;
    let locator = held.locator();
    fixture.bind_turn(&id, &locator, turn);
    // The driver releases its lease on its own once the turn's outcome is
    // unknown, so `held` cannot be the holder. Another process is: its lease
    // is put back unexpired, and the waiter's recycle has to wait it out.
    held.release().await.unwrap();
    reinstate_released_lease(&fixture.database());
    let live = crate::agent::live::ProductLiveChild::new(
        AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: fixture.sessions(),
            opened: None,
            recovered_turn: None,
            locator,
            topology_owner: None,
        },
        turn,
        control,
    );
    assert!(
        fixture
            .runtime
            .try_occupy_live(id.clone(), Box::new(live))
            .await
            .is_ok()
    );
    spawn_topology_turn_waiter(
        crate::agent::children::TurnWaiterRegistration::register(fixture.runtime.driver(), &id)
            .unwrap(),
        Arc::clone(&fixture.runtime.live),
        None,
        None,
        handle,
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        while !fixture.runtime.is_recycling(&id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the waiter starts its recycle");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(fixture.runtime.is_recycling(&id), "the recycle is waiting");

    let started = std::time::Instant::now();
    let (succeeded, refused) = fixture.recover_agent(&id).await;
    assert!(!succeeded, "{refused}");
    assert_eq!(code(&refused), "recovery_owner_live", "{refused}");
    // The pass gets the reattach lock at once and leaves the child alone.
    let report = run_tree_recovery_pass(&fixture.tree_id).await.unwrap();
    assert!(report.recovered.is_empty(), "{report:?}");
    assert!(
        report
            .refused
            .iter()
            .all(|(_, error)| error.code == TopologyErrorCode::RecoveryOwnerLive),
        "{report:?}"
    );
    assert!(report.not_attempted.is_empty(), "{report:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "recovery waited {:?} behind the recycle",
        started.elapsed()
    );

    release_reinstated_lease(&fixture.database());
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.runtime.is_recycling(&id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the waiter finishes its recycle once the lease is free");
}

#[test]
fn code_cell_scope_refusal_is_not_executor_evidence_refusal() {
    use plakous::sessions::SessionRecoveryAction::{
        EffectEvidenceRequired, EvaluationAbandonmentRequired, FinalizationRequired,
        ProvenNotDispatched, ProviderEvidenceRequired,
    };
    assert_eq!(
        recovery_required_code([
            ProvenNotDispatched,
            FinalizationRequired,
            EvaluationAbandonmentRequired
        ]),
        TopologyErrorCode::RecoverySessionUnavailable
    );
    assert_eq!(
        recovery_required_code([EffectEvidenceRequired]),
        TopologyErrorCode::RecoveryEffectEvidenceRefused
    );
    assert_eq!(
        recovery_required_code([ProviderEvidenceRequired]),
        TopologyErrorCode::RecoveryEffectEvidenceRefused
    );
}

#[tokio::test]
async fn fenced_waiter_recycle_of_quiescent_head_settles_as_failed() {
    let fixture = Fixture::new("recover-quiescent-recycle");
    let id = fixture.spawn();
    let (held, handle, control) = fixture.start_failing_turn(&fixture.sessions(), &id).await;
    let turn = handle.turn_id();
    Fixture::await_unknown_outcome(&handle).await;
    let locator = held.release().await.unwrap();
    let locator = fixture.quiescent_after_reopen(locator).await;
    assert!(fixture.sessions().head_is_quiescent(locator).await.unwrap());
    fixture.bind_turn(&id, &locator, turn);
    let live = crate::agent::live::ProductLiveChild::new(
        AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: fixture.sessions(),
            opened: None,
            recovered_turn: None,
            locator,
            topology_owner: None,
        },
        turn,
        control,
    );
    assert!(
        fixture
            .runtime
            .try_occupy_live(id.clone(), Box::new(live))
            .await
            .is_ok()
    );
    spawn_topology_turn_waiter(
        crate::agent::children::TurnWaiterRegistration::register(fixture.runtime.driver(), &id)
            .unwrap(),
        Arc::clone(&fixture.runtime.live),
        None,
        None,
        handle,
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.runtime.live.lock().await.contains_key(&id)
            || fixture.runtime.is_recycling(&id)
            || fixture.status(&id) == NodeStatus::Running
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    assert_eq!(fixture.failure_kind(&id).as_deref(), Some(TURN_ABANDONED));
}

#[tokio::test]
async fn recover_agent_finalization_debt_settles_as_recovered_without_turn() {
    let fixture = Fixture::new("recover-finalization-debt-no-turn");
    let id = fixture.spawn();
    let (held, handle, _) = fixture.start_failing_turn(&fixture.sessions(), &id).await;
    Fixture::await_unknown_outcome(&handle).await;
    let locator = held.release().await.unwrap();
    let locator = plakous_offline_crash_harness::seed_finalization_debt(
        &fixture.database(),
        locator,
        handle.turn_id(),
    )
    .await
    .unwrap();
    assert!(!fixture.sessions().head_is_quiescent(locator).await.unwrap());
    let debt = AgentSessions::inspect_recovery_sqlite(fixture.database(), locator)
        .unwrap()
        .expect("real Working head must have finalization obligations");
    assert_eq!(
        debt.phase(),
        plakous::sessions::SessionRecoveryPhase::Finalization
    );
    fixture.bind_turn(&id, &locator, handle.turn_id());
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.record_turn_failure(
                &id,
                crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                "delivered terminal with no unfinished work".into(),
            )
            .map_err(|error| error.message)
        })
        .unwrap();
    let (succeeded, receipt) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{receipt}");
    assert_eq!(receipt["recovered"], true);
    assert_eq!(receipt["failure_kind"], RECOVERED_WITHOUT_TURN);
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    let closed = fixture
        .runtime
        .read(|tree| tree.node(&id).unwrap().session_locator.clone().unwrap())
        .unwrap()
        .parse::<SessionLocator>()
        .unwrap();
    assert_ne!(closed.checkpoint_id(), locator.checkpoint_id());
    assert!(fixture.sessions().head_is_quiescent(closed).await.unwrap());
    assert!(
        AgentSessions::inspect_recovery_sqlite(fixture.database(), closed)
            .unwrap()
            .is_none()
    );
    let unchanged = fixture.quiescent_after_reopen(closed).await;
    assert_eq!(
        unchanged, closed,
        "second open must not write another checkpoint"
    );
    let (succeeded, repeated) = fixture.recover_agent(&id).await;
    assert!(succeeded, "{repeated}");
    assert_eq!(repeated["recovered"], false);
}

#[tokio::test]
async fn automatic_recovery_settles_a_followup_hold_without_an_admitted_start() {
    let fixture = Fixture::new("followup-hold-no-start");
    let id = fixture.spawn();
    let sessions = fixture.sessions();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("new child session required")
    };
    let locator = session.release().await.unwrap();
    assert!(sessions.head_is_quiescent(locator).await.unwrap());
    mutate_and_publish(&fixture.runtime, &id, |tree| {
        tree.record_child_session(&id, &locator.to_string())
    })
    .unwrap();
    let feed = read_tree_feed(&sessions, locator.session_id(), 0)
        .await
        .unwrap();
    mutate_and_publish(&fixture.runtime, &id, |tree| feed.apply(tree, &id)).unwrap();
    fixture
        .runtime
        .record_then_apply(&id, |tree| tree.record_host_cancellation(&id))
        .unwrap();
    let opening = mutate_and_publish(&fixture.runtime, &id, |tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
                agent_id: id.clone(),
                message: "followup that never reached Start".into(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
    })
    .unwrap();
    fixture
        .runtime
        .record_then_apply(&id, |tree| {
            tree.record_fault(&id, "followup Start submission outcome is unknown");
            tree.record_host_materialization(&id)
        })
        .unwrap();
    assert_eq!(fixture.status(&id), NodeStatus::RecoveryRequired);
    let opened = fixture
        .runtime
        .read(|tree| tree.attempt(&id).unwrap().attempt_id.clone())
        .unwrap();
    let cursor = fixture
        .runtime
        .read(|tree| tree.feed_cursor(&locator.session_id().to_string()))
        .unwrap();
    let page = sessions
        .feed_page(plakous::sessions::feed::FeedPageRequest::after(
            plakous::sessions::feed::FeedCursor::new(
                locator.session_id(),
                plakous::sessions::feed::FeedSeq::new(cursor),
            ),
        ))
        .await
        .unwrap();
    assert!(
        matches!(page, plakous::sessions::feed::FeedPage::Records { records, .. } if records.is_empty())
    );
    let port = ParentSessionPort {
        parent: fixture.parent.clone(),
    };
    let report = recover_unresolved_children(
        &fixture.runtime,
        &port,
        tokio::time::Instant::now() + Duration::from_secs(30),
    )
    .await;
    assert_eq!(report.recovered, vec![id.clone()]);
    assert_eq!(report.refused, [] as [(String, TopologyError); 0]);
    assert_eq!(report.not_attempted, [] as [String; 0]);
    assert_eq!(fixture.dispatches(), 0);
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&id).unwrap();
            assert_eq!(attempt.attempt_id, opened);
            assert_eq!(attempt.status, AttemptStatus::Failed);
            assert_eq!(
                attempt.failure.as_ref().unwrap().kind,
                RECOVERED_WITHOUT_TURN
            );
            assert!(attempt.turn_id.is_none());
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Failed);
            assert!(matches!(
                tree.recovery_need(&id).unwrap(),
                RecoveryNeed::NotRequired
            ));
            assert_eq!(
                tree.mail(&opening.message_id).unwrap().durable,
                crate::agent_tree::MailDurableState::Acknowledged
            );
            assert!(tree.pending_result_mail("root").iter().any(|mail| {
                let body: serde_json::Value = serde_json::from_str(&mail.body).unwrap();
                mail.from == id
                    && body["attempt_id"] == opened.as_str()
                    && body["status"] == "failed"
            }));
        })
        .unwrap();
    let next = mutate_and_publish(&fixture.runtime, &id, |tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
                agent_id: id.clone(),
                message: "new followup only".into(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
    })
    .unwrap();
    let replay_feed = read_tree_feed(&sessions, locator.session_id(), 0)
        .await
        .unwrap();
    fixture
        .runtime
        .read(|tree| {
            assert_ne!(tree.attempt(&id).unwrap().attempt_id, opened);
            assert_eq!(
                tree.pending_mail(&id)
                    .iter()
                    .map(|mail| &mail.message_id)
                    .collect::<Vec<_>>(),
                vec![&next.message_id]
            );
            assert!(
                tree.wake_mail(&id)
                    .iter()
                    .all(|mail| mail.message_id != opening.message_id)
            );
            let mut rebuilt = tree.clone();
            rebuilt.clear_feed_views();
            let root_session = rebuilt.tree_id.clone();
            rebuilt
                .project_feed(
                    "root",
                    &root_session,
                    &[],
                    &crate::agent_tree::TreeFeedReads::default(),
                )
                .unwrap();
            replay_feed.apply(&mut rebuilt, &id).unwrap();
            rebuilt.fold_due(&id).unwrap();
            rebuilt.fold_due(&id).unwrap();
            assert_eq!(
                rebuilt.node(&id).unwrap().attempt_id,
                tree.node(&id).unwrap().attempt_id
            );
            assert_eq!(rebuilt.pending_mail(&id), tree.pending_mail(&id));
            assert!(
                rebuilt
                    .wake_mail(&id)
                    .iter()
                    .all(|mail| mail.message_id != opening.message_id)
            );
        })
        .unwrap();
}

/// Clears the reopen-window hook even when the test panics mid-delivery.
struct ReopenSlotHook(String);

impl Drop for ReopenSlotHook {
    fn drop(&mut self) {
        crate::agent::park::hooks::clear_around_reopen_spawned_child(&self.0);
    }
}

/// A mail Start whose live check sees a vacant slot, and whose slot is taken
/// after `reopen_spawned_child` returns and before `reserve_startup_admitted`,
/// answers `SlotBusy` and does not fault the child.
#[tokio::test]
async fn a_start_whose_slot_is_taken_during_reopen_answers_slot_busy() {
    let fixture = Fixture::new("slot-taken-during-reopen");
    let mut roster = crate::orchestration::RosterInput::test_working_set("selected-tuple");
    let review = crate::agent_tree_protocol::TemplateName::parse("review").unwrap();
    roster.templates.get_mut(&review).unwrap().account = "codex".into();
    roster.bind_hints = crate::orchestration::BindHints::from_names(
        &[crate::orchestration::RosterInput::TEST_ACCOUNT, "codex"],
        &[],
    );
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.install_snapshot(crate::orchestration::OrchestrationSnapshot::freeze(roster));
            Ok(())
        })
        .unwrap();
    let id = fixture.spawn();
    let sessions = fixture.sessions();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("created child session required");
    };
    let locator = session.release().await.unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.bind_session_locator(&id, locator.to_string())
                .map_err(|error| error.message)
        })
        .unwrap();
    let held = Arc::new(Mutex::new(None::<crate::topology_runtime::ChildStartup>));
    let (runtime, child, slot) = (Arc::clone(&fixture.runtime), id.clone(), Arc::clone(&held));
    let _clear = ReopenSlotHook(id.clone());
    crate::agent::park::hooks::set_around_reopen_spawned_child(
        &id,
        Arc::new(move || {
            let (runtime, child, slot) = (Arc::clone(&runtime), child.clone(), Arc::clone(&slot));
            Box::pin(async move {
                if let Ok(startup) = runtime.reserve_startup(&child).await {
                    *slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(startup);
                }
            })
        }),
    );
    let path = fixture
        .runtime
        .read(|tree| tree.node(&id).unwrap().path.display().to_string())
        .unwrap();
    let deliverer = crate::agent::children::ProductMailDeliverer {
        parent: fixture.parent.clone(),
        runtime: Arc::clone(&fixture.runtime),
    };
    let outcome = crate::agent_tree_tools::MailDeliverer::deliver(
        &deliverer,
        crate::agent_tree_tools::MailDeliveryRequest {
            agent_id: id.clone(),
            session_locator: locator.to_string(),
            disposition: crate::agent_tree::MailDisposition::Started,
            message_id: "m-during-reopen".into(),
            body: "use the v2 schema".into(),
            description: String::new(),
            prompt: String::new(),
            output_schema_json: String::new(),
            path,
            turn_token: 1,
        },
    )
    .await;
    let (fault, status) = fixture
        .runtime
        .read(|tree| {
            (
                tree.fault_reason(&id).map(str::to_owned),
                tree.node(&id).unwrap().status,
            )
        })
        .unwrap();
    assert!(
        held.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some(),
        "the other start did not take the slot during reopen"
    );
    assert_eq!(
        outcome.map(|ack| ack.turn_id),
        Err(crate::agent_tree_tools::MailDeliveryFailure::SlotBusy)
    );
    assert_eq!(fault, None, "the slot taken during reopen faulted the node");
    assert_ne!(status, NodeStatus::RecoveryRequired);
}

/// An interrupted child whose attempt still binds a turn its quiescent
/// session no longer runs, with no live slot: what a turn waiter leaves
/// before it releases the cancelled turn, or forever once no waiter is left.
async fn interrupted_child_with_an_ended_bound_turn(fixture: &Fixture) -> (String, SessionLocator) {
    let id = fixture.spawn();
    let CreateSessionOutcome::Created { session, .. } = fixture
        .sessions()
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            fixture.binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("created child session required");
    };
    let locator = session.release().await.unwrap();
    fixture.bind_turn(
        &id,
        &locator,
        "00000000-0000-0000-0000-0000000000a1"
            .parse::<plakous::model::TurnId>()
            .unwrap(),
    );
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: id.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .map(|_| ())
            .map_err(|error| error.message)
        })
        .unwrap();
    assert_eq!(fixture.status(&id), NodeStatus::Interrupted);
    (id, locator)
}

async fn deliver_queued(
    fixture: &Fixture,
    id: &str,
    locator: &SessionLocator,
) -> Result<crate::agent_tree_tools::MailDeliveryAck, crate::agent_tree_tools::MailDeliveryFailure>
{
    let path = fixture
        .runtime
        .read(|tree| tree.node(id).unwrap().path.display().to_string())
        .unwrap();
    let deliverer = crate::agent::children::ProductMailDeliverer {
        parent: fixture.parent.clone(),
        runtime: Arc::clone(&fixture.runtime),
    };
    crate::agent_tree_tools::MailDeliverer::deliver(
        &deliverer,
        crate::agent_tree_tools::MailDeliveryRequest {
            agent_id: id.to_owned(),
            session_locator: locator.to_string(),
            disposition: crate::agent_tree::MailDisposition::Queued,
            message_id: "m-after-unwind".into(),
            body: "use the v2 schema".into(),
            description: String::new(),
            prompt: String::new(),
            output_schema_json: String::new(),
            path,
            turn_token: 1,
        },
    )
    .await
}

/// Mail to an interrupted child whose bound turn no turn waiter watches
/// fails closed at once (`operation_recovery_required`, the child
/// `recovery_required`) instead of waiting out the send's 60 s budget for a
/// release nobody will make (spec 03 section 9).
#[tokio::test]
async fn mail_to_an_interrupted_turn_no_waiter_watches_fails_closed_at_once() {
    let fixture = Fixture::new("unwatched-interrupted-turn");
    let (id, _) = interrupted_child_with_an_ended_bound_turn(&fixture).await;
    let (sent, receipt) = fixture
        .invoke(
            "send_agent_message",
            json!({ "agent_id": id, "message": "use the v2 schema" }),
        )
        .await;
    assert!(!sent, "{receipt}");
    assert_eq!(code(&receipt), "operation_recovery_required", "{receipt}");
    assert_eq!(fixture.status(&id), NodeStatus::RecoveryRequired);
}

/// The delivery under that send: no waiter, so `RecoveryRequired`, not the
/// `TurnEnded` that makes the send wait for an unwind.
#[tokio::test]
async fn an_interrupted_turn_no_waiter_watches_is_recovery_required() {
    let fixture = Fixture::new("unwatched-interrupted-delivery");
    let (id, locator) = interrupted_child_with_an_ended_bound_turn(&fixture).await;
    let outcome = deliver_queued(&fixture, &id, &locator).await;
    assert!(
        matches!(
            outcome,
            Err(crate::agent_tree_tools::MailDeliveryFailure::RecoveryRequired(_))
        ),
        "{outcome:?}"
    );
}

/// While its turn waiter still runs, the same delivery answers `TurnEnded`:
/// the waiter releases the cancelled turn, and the send resumes the child
/// with its mail once it has.
#[tokio::test]
async fn an_interrupted_turn_its_waiter_still_watches_waits_for_the_release() {
    let fixture = Fixture::new("watched-interrupted-delivery");
    let (id, locator) = interrupted_child_with_an_ended_bound_turn(&fixture).await;
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.enter_turn_waiter(&id);
            Ok(())
        })
        .unwrap();
    let outcome = deliver_queued(&fixture, &id, &locator).await;
    assert_eq!(
        outcome.map(|ack| ack.turn_id),
        Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded)
    );
    assert_eq!(fixture.status(&id), NodeStatus::Interrupted);
}

/// Spec 03 §14.3: a child behind its feed's retention seeds its session
/// locator from the head of its branch the behind-retention page named; a
/// head of another branch, or the root, is left.
#[test]
fn a_child_behind_retention_seeds_its_locator_from_its_branch_head() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
    let locator = |branch: u32, checkpoint: u32| -> SessionLocator {
        format!(
            "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-{branch:012x}/\
             00000000-0000-4000-8000-{checkpoint:012x}"
        )
        .parse()
        .unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        "00000000-0000-4000-8000-0000000000aa",
        "root",
        "owner",
        dir.path().to_owned(),
        "selected",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, locator(1, 1).to_string())
        .unwrap();
    let feed = TreeFeed {
        session_id: locator(1, 1).session_id().to_string(),
        retained_from: 30,
        below_retained: Some(40),
        heads: vec![locator(2, 9), locator(1, 7)],
        head_turns: std::collections::BTreeMap::from([(
            locator(1, 7),
            Some("00000000-0000-4000-8000-000000000077".parse().unwrap()),
        )]),
        records: Vec::new(),
        reads: crate::agent_tree::TreeFeedReads::default(),
    };
    assert!(feed.went_behind_retention());
    feed.apply(&mut tree, &child).unwrap();
    assert_eq!(
        tree.node(&child).unwrap().session_locator.as_deref(),
        Some(locator(1, 7).to_string().as_str())
    );
    assert_eq!(tree.feed_cursor(&feed.session_id), 40);
    assert_eq!(
        tree.attempt(&child).unwrap().turn_id.as_deref(),
        Some("00000000-0000-4000-8000-000000000077")
    );
    assert!(tree.node(&child).unwrap().active_turn);
    assert_eq!(tree.attempt(&child).unwrap().usage.turns, 0);
    let bound = tree.attempt(&child).unwrap().clone();
    feed.apply(&mut tree, &child).unwrap();
    assert_eq!(tree.attempt(&child).unwrap(), &bound);
    let mut quiescent = feed;
    // A later retention boundary supplies a new seed, not a rewrite of
    // already-folded head evidence at the old position.
    quiescent.retained_from = 42;
    quiescent.below_retained = Some(41);
    quiescent.heads = vec![locator(1, 8)];
    quiescent.head_turns = std::collections::BTreeMap::from([(locator(1, 8), None)]);
    quiescent.apply(&mut tree, &child).unwrap();
    assert!(!tree.node(&child).unwrap().active_turn);
    assert!(tree.attempt(&child).unwrap().turn_id.is_none());
    let root_locator = tree.node("root").unwrap().session_locator.clone();
    quiescent.apply(&mut tree, "root").unwrap();
    assert_eq!(tree.node("root").unwrap().session_locator, root_locator);
}

#[tokio::test]
async fn refold_reread_spans_more_than_sixty_four_synthetic_pages() {
    use plakous::sessions::feed::{FeedPage, FeedRecord, FeedSeq, MAX_FEED_PAGE_RECORDS};
    let session = "00000000-0000-4000-8000-0000000000f6".parse().unwrap();
    let count = 65 * MAX_FEED_PAGE_RECORDS + 1;
    let records: Vec<_> = (1..=count).map(|seq| {
        let header = serde_json::from_value(json!({
            "session_id": session, "seq": seq,
            "branch_id": "00000000-0000-4000-8000-00000000000b",
            "committed_at_ms": seq,
            "change": {"created": {"genesis": "00000000-0000-4000-8000-000000003001", "revision": seq}}
        })).unwrap();
        FeedRecord::from_store(header, Vec::new())
    }).collect();
    let feed = read_tree_feed_pages(session, 0, |after| {
        let page = FeedPage::Records {
            records: records
                .iter()
                .skip(after as usize)
                .take(MAX_FEED_PAGE_RECORDS)
                .cloned()
                .collect(),
            retained_from: FeedSeq::new(1),
            tail: FeedSeq::new(count as u64),
        };
        std::future::ready(Ok::<_, String>(page))
    })
    .await
    .unwrap();
    assert_eq!(
        feed.records.last().unwrap().header().seq.get(),
        count as u64
    );
    let fixture = Fixture::new(&session.to_string());
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            let root = tree.root_agent_id.clone();
            let projection = feed.apply(tree, &root).unwrap();
            assert_eq!(projection.cursor, count as u64);
            Ok(())
        })
        .unwrap();
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn refold_reread_refuses_a_short_run_at_the_record_cap() {
    use plakous::sessions::feed::{
        FeedPage, FeedRecord, FeedSeq, MAX_FEED_PAGE_RECORDS, MAX_FEED_RECORDS_PER_SESSION,
    };
    let session = "00000000-0000-4000-8000-0000000000f6".parse().unwrap();
    let target = MAX_FEED_RECORDS_PER_SESSION + 1;
    let feed = read_tree_feed_pages(session, 0, |after| {
        let records = ((after + 1)..=target)
            .take(MAX_FEED_PAGE_RECORDS)
            .map(|seq| {
                let header = serde_json::from_value(json!({
                    "session_id": session, "seq": seq,
                    "branch_id": "00000000-0000-4000-8000-00000000000b",
                    "committed_at_ms": seq,
                    "change": {"created": {"genesis": "00000000-0000-4000-8000-000000003001", "revision": seq}}
                })).unwrap();
                FeedRecord::from_store(header, Vec::new())
            })
            .collect();
        std::future::ready(Ok::<_, String>(FeedPage::Records {
            records,
            retained_from: FeedSeq::new(1),
            tail: FeedSeq::new(target),
        }))
    })
    .await;
    assert!(
        feed.err()
            .unwrap()
            .contains("did not reach its initial tail")
    );
}

#[tokio::test]
async fn cursor_read_only_pages_new_records_of_a_ten_thousand_record_feed() {
    use plakous::sessions::feed::{FeedPage, FeedRecord, FeedSeq, MAX_FEED_PAGE_RECORDS};
    let session: plakous::model::SessionId =
        "00000000-0000-4000-8000-0000000000f6".parse().unwrap();
    let cursor = 10_000;
    let fixture = Fixture::new(&session.to_string());
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.seed_feed_cursor(&session.to_string(), cursor);
            Ok(())
        })
        .unwrap();
    let cursor = fixture
        .runtime
        .read(|tree| tree.feed_cursor(&session.to_string()))
        .unwrap();
    let target = cursor + 3;
    let mut requested = Vec::new();
    let feed = read_tree_feed_pages(session, cursor, |after| {
        requested.push(after);
        let records = ((after + 1)..=target)
            .take(MAX_FEED_PAGE_RECORDS)
            .map(|seq| {
                let header = serde_json::from_value(json!({
                    "session_id": session, "seq": seq,
                    "branch_id": "00000000-0000-4000-8000-00000000000b",
                    "committed_at_ms": seq,
                    "change": {"created": {"genesis": "00000000-0000-4000-8000-000000003001", "revision": seq}}
                })).unwrap();
                FeedRecord::from_store(header, Vec::new())
            }).collect();
        std::future::ready(Ok::<_, String>(FeedPage::Records {
            records, retained_from: FeedSeq::new(1), tail: FeedSeq::new(target),
        }))
    }).await.unwrap();
    assert_eq!(requested, [cursor]);
    assert_eq!(feed.records.len(), 3);
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}
