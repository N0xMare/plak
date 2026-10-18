use super::bind::{StartupWorker, child_create_failure};
use super::reopen::reconcile_reopened_child_results;
use super::turns::await_child_start;
use super::waiter::{start_submit_harness_turn, wait_transient_gate};
use super::*;
use crate::topology_runtime::{LiveChildHandle, LiveRelease, ProductTopologyRuntime};
use plakous::agent::TurnControl;

struct CleanupChild {
    turn: TurnId,
    failure: bool,
}

impl LiveChildHandle for CleanupChild {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        None
    }
    fn turn_id(&self) -> TurnId {
        self.turn
    }
    fn turn_control(&self) -> TurnControl {
        panic!("unused fixture control")
    }
    fn follow_up(
        &self,
        _: CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
    fn restart(
        &mut self,
        _: TreeDriver,
        _: CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
        Box::pin(async { Err("unused".into()) })
    }
    fn release(
        self: Box<Self>,
        driver: TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        self.cancel_and_release(driver)
    }
    fn cancel_and_release(
        self: Box<Self>,
        _driver: TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        Box::pin(async move {
            LiveRelease {
                feed: None,
                locator: None,
                failure: self.failure.then(|| "fixture retirement failed".into()),
            }
        })
    }
}

#[tokio::test]
async fn attempt_end_retires_self_and_all_descendants_after_one_retirement_fails() {
    use crate::agent_tree_protocol::SpawnAgentRequest;
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let (parent, children, identity) = runtime
        .mutate_then_emit(false, |tree| {
            let mut snapshot = tree.snapshot.clone();
            snapshot.max_depth = 2;
            snapshot.digest = snapshot.compute_digest();
            tree.install_snapshot(snapshot);
            let parent = tree
                .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                .unwrap()
                .agent_id;
            tree.bind_attempt_turn(&parent, turn.to_string()).unwrap();
            let children = (0..2)
                .map(|_| {
                    tree.spawn_agent(&parent, &SpawnAgentRequest::test_review(), None)
                        .unwrap()
                        .agent_id
                })
                .collect::<Vec<_>>();
            let identity = ChildTurnIdentity {
                attempt: tree.attempt(&parent).unwrap().attempt_id.clone(),
                turn,
            };
            Ok((parent, children, identity))
        })
        .unwrap();
    for (id, failure) in [
        (parent.clone(), false),
        (children[0].clone(), true),
        (children[1].clone(), false),
    ] {
        assert!(
            runtime
                .try_occupy_live(id, Box::new(CleanupChild { turn, failure }))
                .await
                .is_ok()
        );
    }
    assert!(
        end_attempt_work(runtime.driver(), &runtime.live, &parent, &identity)
            .await
            .is_err()
    );
    let live = runtime.live.lock().await;
    assert!(
        !live.contains_key(&parent),
        "own live owner must retire despite descendant failure"
    );
    assert!(
        !live.contains_key(&children[1]),
        "other descendants must still retire"
    );
}

fn runtime(root: &Path) -> Arc<ProductTopologyRuntime> {
    Arc::new(ProductTopologyRuntime::new(
        crate::topology_runtime::test_support::bootstrap("child-retirement", root.to_path_buf()),
        crate::topology_runtime::test_support::memory_store(),
        Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ))
}

struct CloseSaveStore {
    inner: crate::agent_tree::MemoryTreeStore,
    saves: std::sync::atomic::AtomicUsize,
    fail_at: std::sync::atomic::AtomicUsize,
    stale: bool,
    prepared: std::sync::Mutex<Option<crate::agent_tree::PersistedTree>>,
}

#[test]
fn refused_waiter_bind_does_not_unregister_a_concurrent_waiter() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for stale in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(CloseSaveStore {
            inner: crate::agent_tree::MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(0),
            stale,
            prepared: std::sync::Mutex::new(None),
        });
        let runtime = ProductTopologyRuntime::new(
            crate::topology_runtime::test_support::bootstrap(
                "waiter-bind-refusal",
                root.path().to_path_buf(),
            ),
            store.clone(),
            Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
            None,
        );
        let child = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent(
                        "root",
                        &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                        None,
                    )
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        let concurrent = TurnWaiterRegistration::register(runtime.driver(), &child).unwrap();
        store
            .fail_at
            .store(store.saves.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
        let mut registration = None;
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        assert!(
            register_admitted_turn(
                &runtime,
                StartAdmission::Ordinary,
                &child,
                turn,
                &mut registration
            )
            .is_err()
        );
        drop(registration);
        assert!(runtime.read(|tree| tree.has_turn_waiter(&child)).unwrap());
        drop(concurrent);
        assert!(!runtime.read(|tree| tree.has_turn_waiter(&child)).unwrap());
    }
}

impl crate::agent_tree::TreeStore for CloseSaveStore {
    fn exists(&self, id: &str) -> Result<bool, crate::agent_tree::TreeStoreError> {
        crate::agent_tree::TreeStore::exists(&self.inner, id)
    }
    fn ids(&self) -> Result<Vec<String>, crate::agent_tree::TreeStoreError> {
        crate::agent_tree::TreeStore::ids(&self.inner)
    }
    fn load(
        &self,
        id: &str,
    ) -> Result<Option<crate::agent_tree::PersistedTree>, crate::agent_tree::TreeStoreError> {
        crate::agent_tree::TreeStore::load(&self.inner, id)
    }
    fn save(
        &self,
        tree: &crate::agent_tree::ProductAgentTree,
    ) -> Result<crate::agent_tree::Revision, crate::agent_tree::TreeStoreError> {
        use std::sync::atomic::Ordering;
        let count = self.saves.fetch_add(1, Ordering::SeqCst) + 1;
        if count == self.fail_at.load(Ordering::SeqCst) {
            return Err(if self.stale {
                crate::agent_tree::TreeStoreError::stale(tree.stored_revision + 1, tree.revision)
            } else {
                crate::agent_tree::TreeStoreError::unavailable("close consequence refused")
            });
        }
        let revision = crate::agent_tree::TreeStore::save(&self.inner, tree)?;
        if tree
            .operations()
            .values()
            .any(|op| op.tool_name == "seal_effect" && op.terminal_response.is_none())
        {
            *self.prepared.lock().unwrap() =
                crate::agent_tree::TreeStore::load(&self.inner, &tree.tree_id)?;
        }
        Ok(revision)
    }
}

#[tokio::test]
async fn hosted_descendant_close_refusal_and_commit_cut_never_dispatch_unpublished_seals() {
    use crate::agent_tree::{ProductAgentTree, TreeStore as _};
    use crate::agent_tree_protocol::{SpawnAgentRequest, Tier, WriterMode};
    use crate::occupancy_vcs::fake::{FakeOp, FakeVcs};
    use std::sync::atomic::{AtomicUsize, Ordering};
    for spent in [false, true] {
        for stale in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let store = Arc::new(CloseSaveStore {
                inner: crate::agent_tree::MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(0),
                stale,
                prepared: std::sync::Mutex::new(None),
            });
            let tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "hosted-close-cut",
                "root",
                "owner",
                root.path().to_path_buf(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let runtime = ProductTopologyRuntime::new(tree, store.clone(), vcs.clone(), None);
            let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
            let (parent, child, identity) = runtime
                .mutate_then_emit(false, |tree| {
                    let mut snapshot = tree.snapshot.clone();
                    snapshot.max_depth = 2;
                    snapshot.digest = snapshot.compute_digest();
                    tree.install_snapshot(snapshot);
                    let parent = tree
                        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                        .unwrap()
                        .agent_id;
                    tree.bind_attempt_turn(&parent, turn.to_string()).unwrap();
                    let child = tree
                        .spawn_agent(
                            &parent,
                            &SpawnAgentRequest::test_named(
                                "implement",
                                Tier::Mutate,
                                "worker",
                                "task",
                            )
                            .with_fork(),
                            None,
                        )
                        .unwrap()
                        .agent_id;
                    let identity = ChildTurnIdentity {
                        attempt: tree.attempt(&parent).unwrap().attempt_id.clone(),
                        turn,
                    };
                    Ok((parent, child, identity))
                })
                .unwrap();
            let before = runtime
                .read(|tree| {
                    (
                        tree.node(&child).unwrap().clone(),
                        tree.attempt(&child).unwrap().clone(),
                    )
                })
                .unwrap();
            store
                .fail_at
                .store(store.saves.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
            let terminal = crate::agent_tree::ChildTerminal {
                attempt_id: identity.attempt.clone(),
                turn_id: turn.to_string(),
                committed_at_ms: 1000,
                outcome: plakous::sessions::feed::FeedTurnOutcome::Completed {
                    finish: plakous::model::FinishReason::Stop,
                },
                seq: 1,
            };
            let refused = if spent {
                end_spent_attempt(&runtime, &parent, terminal.clone()).await
            } else {
                end_attempt_work(runtime.driver(), &runtime.live, &parent, &identity).await
            };
            assert!(refused.is_err());
            assert!(vcs.calls_of(FakeOp::Seal).is_empty());
            runtime
                .read(|tree| {
                    assert_eq!(tree.node(&child).unwrap(), &before.0);
                    assert_eq!(tree.attempt(&child).unwrap(), &before.1);
                })
                .unwrap();
            // A successful retry publishes the reservation before dispatch. Capture that
            // save as the crash cut between closure and the effect-result publication.
            // end_attempt_work records the parent's retirement on refusal;
            // the released/spent recovery path retries descendant closure.
            end_spent_attempt(&runtime, &parent, terminal)
                .await
                .unwrap();
            assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
            let persisted = store
                .prepared
                .lock()
                .unwrap()
                .take()
                .expect("seal reservation published before dispatch");
            let mut reopened = ProductAgentTree::reopen(persisted);
            reopened.fold_due(&child).unwrap();
            assert!(reopened.take_effects().seals.is_empty());
            reopened.close_live_descendants(&parent).unwrap();
            assert!(reopened.take_effects().seals.is_empty());
            assert!(store.load("hosted-close-cut").unwrap().is_some());
        }
    }
}

#[test]
fn startup_caller_drop_without_runtime_does_not_panic() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let startup = executor
        .block_on(runtime.reserve_startup("root"))
        .unwrap()
        .unwrap();
    let caller = StartupCaller::new(runtime.clone(), "root".into(), startup.clone());
    assert!(tokio::runtime::Handle::try_current().is_err());
    drop(caller);
    assert!(startup.is_cancelled());
    executor
        .block_on(runtime.finish_startup("root", &startup, StartupWorker::failure()))
        .unwrap();
}

#[test]
fn startup_worker_drop_without_runtime_does_not_panic() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let startup = executor
        .block_on(runtime.reserve_startup("root"))
        .unwrap()
        .unwrap();
    let worker = StartupWorker::new(runtime.clone(), "root".into(), startup.clone());
    assert!(tokio::runtime::Handle::try_current().is_err());
    drop(worker);
    executor
        .block_on(runtime.finish_startup("root", &startup, StartupWorker::failure()))
        .unwrap();
}

#[tokio::test]
async fn late_terminal_cannot_mutate_a_replacement_turn() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let old = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let new = TurnId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
    let (child, identity) = runtime
        .mutate_then_emit(false, |tree| {
            let child = tree
                .spawn_agent(
                    "root",
                    &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap()
                .agent_id;
            tree.bind_attempt_turn(&child, old.to_string()).unwrap();
            let identity = ChildTurnIdentity {
                attempt: tree.attempt(&child).unwrap().attempt_id.clone(),
                turn: old,
            };
            tree.bind_attempt_turn(&child, new.to_string()).unwrap();
            Ok((child, identity))
        })
        .unwrap();
    let before = runtime.read(|tree| tree.revision).unwrap();
    finalize_child_turn(
        runtime.driver(),
        &child,
        Some(&identity),
        None,
        false,
        false,
        None,
    );
    finalize_child_failure(
        runtime.driver(),
        &identity,
        &child,
        crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
        "late failure".into(),
        FailedTurnWaits::Keep,
        None,
    )
    .await;
    runtime
        .read(|tree| {
            assert_eq!(tree.revision, before);
            assert_eq!(
                tree.attempt(&child).unwrap().turn_id.as_deref(),
                Some(new.to_string().as_str())
            );
            assert_eq!(
                tree.node(&child).unwrap().status,
                crate::agent_tree_protocol::NodeStatus::Running
            );
        })
        .unwrap();
}

#[tokio::test]
async fn failed_child_snapshot_error_keeps_original_failure_and_partial_candidate() {
    use crate::agent_tree::turn_outcome::FailedTurnDisposition;
    use crate::agent_tree_protocol::{SpawnAgentRequest, Tier};
    use crate::occupancy_vcs::{VcsError, fake::FakeVcs};
    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let runtime = ProductTopologyRuntime::new(
        crate::topology_runtime::test_support::bootstrap(
            "failed-snapshot",
            root.path().to_path_buf(),
        ),
        crate::topology_runtime::test_support::memory_store(),
        vcs.clone(),
        None,
    );
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let (child, identity) = runtime
        .mutate_then_emit(false, |tree| {
            let child = tree
                .spawn_agent(
                    "root",
                    &SpawnAgentRequest::test_named("implement", Tier::Mutate, "implement", "work")
                        .with_fork(),
                    None,
                )
                .unwrap()
                .agent_id;
            tree.bind_attempt_turn(&child, turn.to_string()).unwrap();
            let identity = ChildTurnIdentity {
                attempt: tree.attempt(&child).unwrap().attempt_id.clone(),
                turn,
            };
            Ok((child, identity))
        })
        .unwrap();
    vcs.seal_next_with(Err(VcsError::seal_failed("injected snapshot failure")));
    finalize_child_failure(
        runtime.driver(),
        &identity,
        &child,
        FailedTurnDisposition::Failed {
            kind: "turn_failed",
        },
        "original provider failure".into(),
        FailedTurnWaits::Keep,
        None,
    )
    .await;
    runtime
        .read(|tree| {
            let attempt = tree.attempt(&child).unwrap();
            let failure = attempt.failure.as_ref().unwrap();
            assert_eq!(failure.kind, "turn_failed");
            assert!(failure.message.contains("original provider failure"));
            assert!(
                failure
                    .message
                    .contains("failed worktree snapshot unavailable: injected snapshot failure")
            );
            assert!(attempt.candidate_id.is_some());
        })
        .unwrap();
}

#[tokio::test]
async fn failed_child_snapshot_is_reported_for_forks_only() {
    use crate::agent_tree::turn_outcome::FailedTurnDisposition;
    use crate::agent_tree_protocol::{SpawnAgentRequest, Tier};
    for fork in [false, true] {
        for recovery in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let runtime = runtime(root.path());
            let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
            let (child, identity) = runtime
                .mutate_then_emit(false, |tree| {
                    let mut request = SpawnAgentRequest::test_named(
                        "implement",
                        Tier::Mutate,
                        "implement",
                        "work",
                    );
                    if fork {
                        request = request.with_fork();
                    }
                    let child = tree.spawn_agent("root", &request, None).unwrap().agent_id;
                    tree.bind_attempt_turn(&child, turn.to_string()).unwrap();
                    let identity = ChildTurnIdentity {
                        attempt: tree.attempt(&child).unwrap().attempt_id.clone(),
                        turn,
                    };
                    Ok((child, identity))
                })
                .unwrap();
            finalize_child_failure(
                runtime.driver(),
                &identity,
                &child,
                if recovery {
                    FailedTurnDisposition::RecoveryRequired
                } else {
                    FailedTurnDisposition::Failed {
                        kind: "turn_failed",
                    }
                },
                "provider failure".into(),
                FailedTurnWaits::Keep,
                None,
            )
            .await;
            runtime
                .read(|tree| {
                    let attempt = tree.attempt(&child).unwrap();
                    let message = &attempt.failure.as_ref().unwrap().message;
                    assert!(message.contains("provider failure"));
                    assert_eq!(
                        message.contains("failed worktree snapshot: refs/plak/"),
                        fork
                    );
                    assert_eq!(attempt.candidate_id.is_some(), fork && !recovery);
                })
                .unwrap();
        }
    }
}

#[tokio::test]
async fn submitted_child_uses_authoritative_failed_terminal_instead_of_channel_success() {
    use crate::agent_tree_protocol::{
        AttemptScopedRequest, AttemptStatus, NodeStatus, SpawnAgentRequest, SubmitResultRequest,
        Tier,
    };
    use plakous::model::ModelErrorKind;
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
        SessionOperationId,
    };
    for (kind, publication_fails) in [
        (ModelErrorKind::Protocol, false),
        (ModelErrorKind::OutcomeUnknown, false),
        (ModelErrorKind::QuotaExceeded, false),
        (ModelErrorKind::RateLimited, false),
        (ModelErrorKind::ContextLimit, false),
        (ModelErrorKind::Policy, false),
        (ModelErrorKind::Protocol, true),
    ] {
        for close_first in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let runtime = runtime(root.path());
            let evidence = root.path().join("evidence.jsonl");
            let release = root.path().join("release");
            // A fast failing model may retire the active turn before
            // start() returns. Hold dispatch until its control is captured.
            let controller =
                plakous_offline_crash_harness::BoundaryController::quiet_gated_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::ProviderDispatchOne,
                    &evidence,
                    &release,
                )
                .unwrap();
            let binding = plakous_offline_crash_harness::terminal_failure_runtime_binding(
                root.path(),
                controller,
                kind,
            )
            .unwrap();
            let database = root.path().join("runtime.sqlite3");
            let backend = SessionBackend::sqlite(&database).unwrap();
            let sessions = Arc::new(AgentSessions::new(backend).unwrap());
            let created = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new("terminal-fixture:create").unwrap(),
                    binding,
                ))
                .await
                .unwrap();
            assert!(matches!(&created, CreateSessionOutcome::Created { .. }));
            let CreateSessionOutcome::Created { session, .. } = created else {
                continue;
            };
            let submission = session
                .agent()
                .start(
                    CommandId::new("terminal-fixture:start").unwrap(),
                    "fail terminal",
                )
                .await
                .unwrap();
            let handle = submission.result().unwrap().clone();
            let turn = handle.turn_id();
            let control = session.agent().turn_control(turn).unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                while !plakous_offline_crash_harness::evidence::read_evidence(&evidence)
                    .is_ok_and(|rows| rows.iter().any(|row| row.boundary == plakous_offline_crash_harness::protocol::CrashBoundary::ProviderDispatchOne))
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if publication_fails {
                rusqlite::Connection::open(&database).unwrap().execute_batch(
                    "CREATE TRIGGER refuse_terminal BEFORE INSERT ON checkpoints BEGIN SELECT RAISE(FAIL, 'injected terminal publication failure'); END;"
                ).unwrap();
            }
            std::fs::write(&release, "release").unwrap();
            let terminal = handle.clone().result().await;
            let recovery = kind == ModelErrorKind::OutcomeUnknown || publication_fails;
            if publication_fails {
                assert!(matches!(
                    terminal.as_ref().unwrap().outcome(),
                    TurnResultOutcome::RecoveryRequired(_)
                ));
            } else {
                let terminal = terminal.unwrap();
                assert_eq!(
                    matches!(terminal.outcome(), TurnResultOutcome::RecoveryRequired(_)),
                    recovery
                );
                if !recovery {
                    assert!(matches!(terminal.outcome(), TurnResultOutcome::Failed(_)));
                }
            }
            let child = runtime
                .mutate_then_emit(false, |tree| {
                    let child = tree
                        .spawn_agent(
                            "root",
                            &SpawnAgentRequest::test_named(
                                "implement",
                                Tier::Mutate,
                                "implement",
                                "work",
                            )
                            .with_fork(),
                            None,
                        )
                        .unwrap()
                        .agent_id;
                    tree.bind_attempt_turn(&child, turn.to_string()).unwrap();
                    tree.set_active_turn(&child, true).unwrap();
                    let token = tree.attempt(&child).unwrap().turn_token;
                    tree.submit_result(
                        &child,
                        &SubmitResultRequest {
                            turn_token: token,
                            output_json: r#"{"ok":true}"#.into(),
                        },
                        None,
                    )
                    .unwrap();
                    if close_first {
                        tree.close_agent(
                            "root",
                            &AttemptScopedRequest {
                                agent_id: child.clone(),
                                expected_attempt_id: None,
                            },
                            None,
                        )
                        .unwrap();
                    }
                    Ok(child)
                })
                .unwrap();
            let locator = session.locator();
            let registry = Arc::new(crate::jobs::JobRegistry::default());
            let environment = plakous::tools::ExecutionEnvironment::host(
                plakous::tools::ExecutionEnvironmentScope::Run(
                    plakous::model::RunId::from_str("00000000-0000-0000-0000-000000000706")
                        .unwrap(),
                ),
                root.path(),
                ".",
                plakous::tools::ExecutionEnvironmentConfig::default(),
            )
            .unwrap();
            let job_host = registry.for_agent(child.clone(), "s");
            let service = job_host
                .attach(
                    plakous::tools::JobOwner {
                        session_id: locator.session_id(),
                        branch_id: locator.branch_id(),
                        agent_id: plakous::model::AgentId::from_str(
                            "00000000-0000-0000-0000-000000000707",
                        )
                        .unwrap(),
                    },
                    environment.joined_process_sessions().unwrap(),
                )
                .unwrap();
            let job = service
                .start(
                    plakous::tools::JoinedProcessStartRequest::new(
                        plakous::tools::JoinedProcessCommand::shell(
                            plakous::tools::JoinedProcessShellProfile::new(
                                "/bin/sh",
                                ["-c".into()],
                            )
                            .unwrap(),
                            "exec sleep 3600",
                        )
                        .unwrap(),
                        ".",
                        Vec::new(),
                        false,
                        plakous::tools::JoinedProcessTerminalMode::Pipes,
                    )
                    .unwrap(),
                    plakous::tools::ExecutionOperationControl::new(
                        plakous::model::CancellationToken::new(),
                    ),
                )
                .unwrap();
            plakous_tools::internal::promote(&service, job).unwrap();
            runtime
                .mutate_then_emit(false, |tree| {
                    tree.bind_session_locator(&child, locator.to_string())
                        .unwrap();
                    Ok(())
                })
                .unwrap();
            crate::agent_tree_tools::invoke_granted_topology_tool_on(
                runtime.driver().clone(),
                "root".into(),
                "list_agents",
                &serde_json::json!({}),
                "bind-test-jobs".into(),
                crate::agent_tree_tools::TopologyHostPorts {
                    job_host: Some(registry.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(registry.snapshot()[0].owner, child);
            let feed = ChildFeedSource {
                sessions: Arc::clone(&sessions),
                session: locator.session_id(),
            };
            let live_child = super::super::live::ProductLiveChild::new(
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
                runtime
                    .try_occupy_live(child.clone(), Box::new(live_child))
                    .await
                    .is_ok()
            );
            spawn_topology_turn_waiter(
                TurnWaiterRegistration::register(runtime.driver(), &child).unwrap(),
                runtime.live.clone(),
                None,
                Some(feed),
                handle,
            );
            tokio::time::timeout(Duration::from_secs(3), async {
                while !runtime.live.lock().await.is_empty()
                    || runtime
                        .read(|tree| {
                            let attempt = tree.attempt(&child).unwrap();
                            !(matches!(
                                attempt.status,
                                AttemptStatus::Failed
                                    | AttemptStatus::Blocked
                                    | AttemptStatus::RecoveryRequired
                            ) || (attempt.status == AttemptStatus::Closed
                                && attempt.failure.is_some()))
                        })
                        .unwrap()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                let state = runtime.read(|tree| {
                    format!(
                        "{:?} {:?} {:?}",
                        tree.attempt(&child).map(|attempt| attempt.status),
                        tree.node(&child).map(|node| node.status),
                        tree.fault_reason(&child)
                    )
                });
                panic!("{kind:?} close_first={close_first}: the attempt never ended: {state:?}")
            });
            assert_eq!(
                service.jobs()[0].status,
                plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::AttemptEnded),
                "failed and recovery-required child jobs must not remain parked",
            );
            assert_eq!(registry.snapshot()[0].owner, child);
            assert_eq!(
                registry.snapshot()[0].status,
                plakous_host_protocol::views::ProcessStatus::Stopped
            );
            runtime
                .read(|tree| {
                    let attempt = tree.attempt(&child).unwrap();
                    assert_eq!(attempt.output_json.as_deref(), Some(r#"{"ok":true}"#));
                    assert_eq!(attempt.usage.turns, 1, "no submit harness turn");
                    assert_eq!(
                        tree.node(&child).unwrap().status,
                        if recovery {
                            NodeStatus::RecoveryRequired
                        } else if close_first {
                            NodeStatus::Closed
                        } else {
                            NodeStatus::Failed
                        }
                    );
                    assert_eq!(
                        attempt.status,
                        if close_first && recovery {
                            AttemptStatus::Closed
                        } else if recovery {
                            AttemptStatus::RecoveryRequired
                        } else if kind == ModelErrorKind::QuotaExceeded {
                            AttemptStatus::Blocked
                        } else {
                            AttemptStatus::Failed
                        }
                    );
                    assert_eq!(attempt.candidate_id.is_some(), !recovery);
                    if kind != ModelErrorKind::QuotaExceeded {
                        assert!(
                            attempt
                                .failure
                                .as_ref()
                                .unwrap()
                                .message
                                .contains("failed worktree snapshot: refs/plak/")
                        );
                    }
                    if kind == ModelErrorKind::RateLimited {
                        assert_eq!(attempt.failure.as_ref().unwrap().kind, "rate_limited");
                    }
                    if kind == ModelErrorKind::ContextLimit && !recovery {
                        let failure = attempt.failure.as_ref().unwrap();
                        assert_eq!(failure.kind, "context_exhausted");
                        assert!(failure.message.contains("respawn with a handoff"));
                        assert!(failure.message.contains("not a follow-up"));
                    }
                    if kind == ModelErrorKind::Policy && !recovery {
                        let failure = attempt.failure.as_ref().unwrap();
                        assert_eq!(failure.kind, "refused");
                        assert!(failure.message.contains("respawn with a handoff"));
                        assert!(failure.message.contains("not a follow-up"));
                        assert!(failure.message.contains("injected terminal model failure"));
                    }
                    if recovery {
                        assert_eq!(
                            attempt.failure.as_ref().unwrap().kind,
                            "turn_recovery_required"
                        );
                    }
                })
                .unwrap();
        }
    }
}

struct EndingChild {
    turn: TurnId,
    entered: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
    failure: bool,
}
impl LiveChildHandle for EndingChild {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        None
    }
    fn turn_id(&self) -> TurnId {
        self.turn
    }
    fn turn_control(&self) -> TurnControl {
        panic!("fixture does not need control")
    }
    fn follow_up(
        &self,
        _: CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Err("fixture".into()) })
    }
    fn restart(
        &mut self,
        _: TreeDriver,
        _: CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
        Box::pin(async {
            Err("duplicate child restart has no live turn; result reconciliation required".into())
        })
    }
    fn release(
        self: Box<Self>,
        _driver: TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.finish.notified().await;
            LiveRelease {
                feed: None,
                locator: (!self.failure).then(|| "completed-head".into()),
                failure: self.failure.then(|| "release refused".into()),
            }
        })
    }
    fn cancel_and_release(
        self: Box<Self>,
        _driver: TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        panic!("terminal retirement must win cancellation")
    }
}

#[tokio::test]
async fn retired_duplicate_restart_marks_child_recovery_required() {
    use crate::agent_tree_protocol::{AttemptStatus, NodeStatus, SpawnAgentRequest};
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let child = runtime
        .mutate_then_emit(false, |tree| {
            let child = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .unwrap();
            tree.bind_attempt_turn(&child.agent_id, turn.to_string())
                .unwrap();
            Ok(child)
        })
        .unwrap();
    runtime
        .try_occupy_live(
            child.agent_id.clone(),
            Box::new(EndingChild {
                turn,
                entered: Arc::new(tokio::sync::Notify::new()),
                finish: Arc::new(tokio::sync::Notify::new()),
                failure: false,
            }),
        )
        .await
        .map_err(|_| "live slot refused")
        .unwrap();
    let identity = ChildTurnIdentity {
        attempt: child.attempt_id,
        turn,
    };
    let error = start_submit_harness_turn(
        runtime.driver(),
        &runtime.live,
        &child.agent_id,
        &identity,
        SubmitNudgeBudget::NONE.next().unwrap(),
    )
    .await
    .err()
    .unwrap();
    assert!(error.contains("result reconciliation required"));
    runtime
        .read(|tree| {
            assert_eq!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert_eq!(
                tree.attempt(&child.agent_id).unwrap().status,
                AttemptStatus::RecoveryRequired
            );
            assert!(
                tree.commands()
                    .values()
                    .any(|command| { command.recipient == child.agent_id && command.unresolved() })
            );
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn transient_gate_interrupt_wins_without_waiting_for_delay() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let child = runtime
        .mutate_then_emit(false, |tree| {
            let child = tree
                .spawn_agent(
                    "root",
                    &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap();
            tree.bind_attempt_turn(&child.agent_id, turn.to_string())
                .unwrap();
            Ok(child)
        })
        .unwrap();
    runtime
        .try_occupy_live(
            child.agent_id.clone(),
            Box::new(EndingChild {
                turn,
                entered: Arc::new(tokio::sync::Notify::new()),
                finish: Arc::new(tokio::sync::Notify::new()),
                failure: false,
            }),
        )
        .await
        .map_err(|_| "live slot refused")
        .unwrap();
    let identity = ChildTurnIdentity {
        attempt: child.attempt_id,
        turn,
    };
    let gate = wait_transient_gate(
        runtime.driver(),
        &runtime.live,
        &child.agent_id,
        &identity,
        crate::agent::transient::ResumeGate::Delay(Duration::from_secs(60)),
    );
    tokio::pin!(gate);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(gate.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    runtime
        .mutate_then_emit(false, |tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: child.agent_id.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .map(|_| ())
            .map_err(|error| error.message)
        })
        .unwrap();
    assert!(!gate.await);
}

#[tokio::test]
async fn submit_harness_receives_results_and_supports_followup_attempts() {
    use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest};
    use plakous::sessions::{AgentSessions, SessionBackend};
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, runtime_binding};

    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let parent = runtime
        .mutate_then_emit(false, |tree| {
            let mut snapshot = tree.snapshot.clone();
            snapshot.max_depth = 2;
            snapshot.digest = snapshot.compute_digest();
            tree.install_snapshot(snapshot);
            let parent = tree
                .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                .unwrap();
            let child = tree
                .spawn_agent(&parent.agent_id, &SpawnAgentRequest::test_review(), None)
                .unwrap();
            tree.submit_result(
                &child.agent_id,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.to_owned(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed(&child.agent_id).unwrap();
            Ok(parent)
        })
        .unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("harness-result.jsonl"),
    )
    .unwrap();
    let sessions = Arc::new(
        AgentSessions::new(
            SessionBackend::sqlite(root.path().join("harness-result.sqlite3")).unwrap(),
        )
        .unwrap(),
    );
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("harness-result-create").unwrap(),
            runtime_binding(root.path(), controller).unwrap(),
        ))
        .await
        .unwrap()
    else {
        panic!("new session required")
    };
    let submission = session
        .agent()
        .start(CommandId::new("harness-result-first").unwrap(), "first")
        .await
        .unwrap();
    let result = submission.result().unwrap().clone();
    let turn_id = result.turn_id();
    let control = session.agent().turn_control(turn_id).unwrap();
    result.result().await.unwrap();
    let locator = session.locator();
    runtime
        .mutate_then_emit(false, |tree| {
            tree.bind_attempt_turn(&parent.agent_id, turn_id.to_string())
                .map_err(|error| error.message)
        })
        .unwrap();
    runtime
        .try_occupy_live(
            parent.agent_id.clone(),
            Box::new(crate::agent::live::ProductLiveChild::new(
                AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: None,
                    sessions,
                    opened: Some(session),
                    recovered_turn: None,
                    locator,
                    topology_owner: None,
                },
                turn_id,
                control,
            )),
        )
        .await
        .map_err(|_| "live slot refused")
        .unwrap();
    let identity = ChildTurnIdentity {
        attempt: parent.attempt_id.clone(),
        turn: turn_id,
    };
    let result = start_submit_harness_turn(
        runtime.driver(),
        &runtime.live,
        &parent.agent_id,
        &identity,
        SubmitNudgeBudget::NONE.next().unwrap(),
    )
    .await
    .unwrap();
    let mut completed_turn = result.turn_id();
    result.result().await.unwrap();
    assert!(
        runtime
            .read(|tree| tree.pending_result_mail(&parent.agent_id).is_empty())
            .unwrap()
    );
    assert!(
        runtime
            .read(|tree| tree.commands().values().any(|command| {
                command.kind == crate::agent_tree::AgentCommandKind::Corrective
                    && command.text.contains("child_result")
                    && command.fact == Some(crate::agent_tree::CommandFact::Accepted)
            }))
            .unwrap()
    );
    // The same node and host generation must admit both corrective rounds
    // again after followup rotates the assignment and its submission token.
    for assignment in 0..2 {
        let first_round = if assignment == 0 { 2 } else { 1 };
        for round in first_round..=2 {
            let identity = ChildTurnIdentity {
                attempt: runtime
                    .read(|tree| tree.attempt(&parent.agent_id).unwrap().attempt_id.clone())
                    .unwrap(),
                turn: completed_turn,
            };
            let mut nudge = SubmitNudgeBudget::NONE;
            for _ in 0..round {
                nudge = nudge.next().unwrap();
            }
            let result = start_submit_harness_turn(
                runtime.driver(),
                &runtime.live,
                &parent.agent_id,
                &identity,
                nudge,
            )
            .await
            .expect("each assignment owns its corrective Start identities");
            completed_turn = result.turn_id();
            result.result().await.unwrap();
            let (command, prompt, operations_before) = runtime
                .read(|tree| {
                    let suffix = format!(":{round}");
                    let command = tree
                        .commands()
                        .values()
                        .find(|command| {
                            command.recipient == parent.agent_id
                                && command.kind == crate::agent_tree::AgentCommandKind::Corrective
                                && command.attempt_id.as_ref() == Some(&identity.attempt)
                                && command.command_id.ends_with(&suffix)
                        })
                        .unwrap();
                    (
                        command.command_id.clone(),
                        submit_harness_prompt(
                            round,
                            tree.attempt(&parent.agent_id).unwrap().turn_token,
                            &tree.node(&parent.agent_id).unwrap().output_schema_json,
                        ),
                        tree.commands().len(),
                    )
                })
                .unwrap();
            super::super::result_context::prepare(
                runtime.driver(),
                &parent.agent_id,
                &command,
                &prompt,
            )
            .expect("same-attempt corrective context replays exactly");
            super::super::result_context::acknowledge(
                runtime.driver(),
                &command,
                super::super::result_context::Submission::Recorded,
            )
            .unwrap();
            assert_eq!(
                runtime.read(|tree| tree.commands().len()).unwrap(),
                operations_before
            );
        }
        runtime
            .mutate_then_emit(false, |tree| {
                assert_eq!(
                    tree.attempt(&parent.agent_id)
                        .unwrap()
                        .submit_harness_nudge
                        .rounds(),
                    2
                );
                tree.record_turn_completed(&parent.agent_id).unwrap();
                Ok(())
            })
            .unwrap();
        if assignment == 0 {
            runtime
                .mutate_then_emit(false, |tree| {
                    tree.send_agent_message(
                        "root",
                        &crate::agent_tree_protocol::SendAgentMessageRequest {
                            agent_id: parent.agent_id.clone(),
                            message: "followup assignment".into(),
                            purpose: Some(crate::agent_tree_protocol::MessagePurpose::Followup),
                            priority: None,
                            in_reply_to: None,
                            output_schema_json: None,
                        },
                        None,
                    )
                    .unwrap();
                    assert_ne!(
                        tree.attempt(&parent.agent_id).unwrap().attempt_id,
                        parent.attempt_id
                    );
                    Ok(())
                })
                .unwrap();
            let result = runtime
                .live
                .lock()
                .await
                .get_mut(&parent.agent_id)
                .unwrap()
                .restart(
                    runtime.driver().clone(),
                    CommandId::new("harness-followup-start").unwrap(),
                    "followup assignment".into(),
                )
                .await
                .unwrap();
            completed_turn = result.turn_id();
            result.result().await.unwrap();
            runtime
                .mutate_then_emit(false, |tree| {
                    tree.bind_attempt_turn(&parent.agent_id, completed_turn.to_string())
                        .unwrap();
                    Ok(())
                })
                .unwrap();
        }
    }
    crate::topology_runtime::retire_live(
        runtime.driver(),
        &runtime.live,
        &parent.agent_id,
        None,
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn reopened_child_reconciles_durable_result_command_without_redispatch() {
    assert_reopened_result_reconciliation(false, false).await;
}

#[tokio::test]
async fn reopened_corrective_child_reconciles_prior_bound_turn_without_redispatch() {
    assert_reopened_result_reconciliation(true, false).await;
}

#[tokio::test]
async fn reopened_corrective_child_without_results_preserves_round_budget() {
    assert_reopened_result_reconciliation(true, true).await;
}

async fn assert_reopened_result_reconciliation(corrective: bool, empty: bool) {
    use crate::agent_tree::{MemoryTreeStore, ProductAgentTree, TreeStore as _};
    use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest};
    use plakous::sessions::{AgentSessions, SessionBackend};
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, runtime_binding};

    let root = tempfile::tempdir().unwrap();
    let tree_store = Arc::new(MemoryTreeStore::new());
    let runtime = ProductTopologyRuntime::new(
        crate::topology_runtime::test_support::bootstrap("result-recovery", root.path().to_owned()),
        tree_store.clone(),
        Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
        None,
    );
    let parent = runtime
        .mutate_then_emit(false, |tree| {
            let mut snapshot = tree.snapshot.clone();
            snapshot.max_depth = 2;
            snapshot.digest = snapshot.compute_digest();
            tree.install_snapshot(snapshot);
            let parent = tree
                .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                .unwrap();
            let child = tree
                .spawn_agent(&parent.agent_id, &SpawnAgentRequest::test_review(), None)
                .unwrap();
            tree.submit_result(
                &child.agent_id,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.to_owned(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed(&child.agent_id).unwrap();
            Ok(parent)
        })
        .unwrap();
    if empty {
        runtime
            .mutate_then_emit(false, |tree| {
                let ids = tree
                    .pending_result_mail(&parent.agent_id)
                    .into_iter()
                    .map(|mail| mail.message_id.clone())
                    .collect::<Vec<_>>();
                for id in ids {
                    tree.acknowledge_mail(&id).map_err(|error| error.message)?;
                }
                Ok(())
            })
            .unwrap();
    }
    let evidence = root.path().join("result-recovery.jsonl");
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
    let database = root.path().join("result-recovery.sqlite3");
    let sessions =
        Arc::new(AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap());
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("result-recovery-create").unwrap(),
            runtime_binding(root.path(), controller.clone()).unwrap(),
        ))
        .await
        .unwrap()
    else {
        panic!("new session required")
    };
    let prior_turn = if corrective {
        let prior = session
            .agent()
            .start(CommandId::new("result-recovery-prior").unwrap(), "prior")
            .await
            .unwrap();
        let result = prior.result().unwrap();
        result.clone().result().await.unwrap();
        Some(result.turn_id())
    } else {
        None
    };
    // A corrective round's identity names its attempt and round.
    let command = if corrective {
        let attempt = runtime
            .read(|tree| tree.attempt(&parent.agent_id).unwrap().attempt_id.clone())
            .unwrap();
        format!("plak:submit-harness:{}:1:{attempt}:1", parent.agent_id)
    } else {
        "result-recovery-start".to_owned()
    };
    let context = super::super::result_context::prepare(
        runtime.driver(),
        &parent.agent_id,
        &command,
        "continue",
    )
    .unwrap();
    let submission = session
        .agent()
        .start(CommandId::new(command.clone()).unwrap(), context.prompt)
        .await
        .unwrap();
    let result = submission.result().unwrap().clone();
    let turn = result.turn_id();
    result.result().await.unwrap();
    // Durable cut: runtime admitted and tree bound the turn, but the
    // result-context acknowledgement has never run.
    runtime
        .mutate_then_emit(false, |tree| {
            tree.bind_attempt_turn(&parent.agent_id, prior_turn.unwrap_or(turn).to_string())
                .map_err(|error| error.message)
        })
        .unwrap();
    let locator = session.release().await.unwrap();
    drop(sessions);
    let persisted = tree_store.load("result-recovery").unwrap().unwrap();
    drop(runtime);
    let runtime = ProductTopologyRuntime::new(
        ProductAgentTree::reopen(persisted),
        tree_store,
        Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
        None,
    );
    let before = std::fs::read(&evidence).unwrap();
    let sessions =
        Arc::new(AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap());
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("result-recovery-open").unwrap(),
            locator,
            runtime_binding(root.path(), controller).unwrap(),
        ))
        .await
        .unwrap()
    else {
        panic!("quiescent reopened session required")
    };
    let reopened = reconcile_reopened_child_results(
        &runtime,
        &parent.agent_id,
        ChildReopen::Attached {
            session: Box::new(AttachedCodingSession {
                output_bound_rederived: None,
                reachability_probe: None,
                sessions,
                locator: session.locator(),
                opened: Some(session),
                recovered_turn: None,
                topology_owner: None,
            }),
            recovered: None,
        },
        ChildReopenMode::OWNER,
    )
    .await
    .unwrap();
    assert!(!bound_child_turn_matches(&runtime, &parent.agent_id, turn));
    if corrective {
        assert_eq!(
            runtime
                .read(|tree| tree
                    .attempt(&parent.agent_id)
                    .unwrap()
                    .submit_harness_nudge
                    .rounds())
                .unwrap(),
            1
        );
    }
    assert_eq!(
        std::fs::read(&evidence).unwrap(),
        before,
        "reconciliation cannot dispatch provider work"
    );
    assert!(
        runtime
            .read(|tree| tree.pending_result_mail(&parent.agent_id).is_empty())
            .unwrap()
    );
    assert!(
        super::super::result_context::prepare(
            runtime.driver(),
            &parent.agent_id,
            "result-recovery-next",
            "next"
        )
        .is_ok(),
        "recorded acceptance must not leave later commands permanently blocked"
    );
    let ChildReopen::Attached { session, .. } = reopened else {
        panic!("attached session required")
    };
    session.release_opened().await.unwrap();
}

#[tokio::test]
async fn fresh_child_start_retains_control_after_terminal_before_ack() {
    child_start_control_fixture(false, true, false).await;
}

#[tokio::test]
async fn duplicate_child_start_registers_existing_live_turn_without_redispatch() {
    child_start_control_fixture(true, false, false).await;
}

#[tokio::test]
async fn duplicate_child_start_without_live_turn_requires_reconciliation() {
    child_start_control_fixture(true, true, false).await;
}

#[tokio::test]
async fn duplicate_child_start_below_retention_preserves_its_projected_acceptance() {
    child_start_control_fixture(true, true, true).await;
}

#[tokio::test]
async fn reclaimed_child_start_settles_consumed_without_binding() {
    use plakous::model::ModelErrorKind;
    use plakous::sessions::{AgentSessions, PruneHistoryOutcome, SessionBackend};
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let child = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    let evidence = root.path().join("reclaimed-start.jsonl");
    let binding = || {
        terminal_failure_runtime_binding(
            root.path(),
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap(),
            ModelErrorKind::Protocol,
        )
        .unwrap()
    };
    let sessions = Arc::new(
        AgentSessions::new(SessionBackend::sqlite(root.path().join("reclaimed.sqlite3")).unwrap())
            .unwrap(),
    );
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("reclaimed:create").unwrap(),
            binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("created session required");
    };
    let command = CommandId::new("reclaimed:start").unwrap();
    session
        .agent()
        .start(command.clone(), "work")
        .await
        .unwrap()
        .result()
        .unwrap()
        .clone()
        .result()
        .await
        .unwrap();
    for round in 0..17 {
        session
            .agent()
            .start(CommandId::new(format!("prune:{round}")).unwrap(), "work")
            .await
            .unwrap()
            .result()
            .unwrap()
            .clone()
            .result()
            .await
            .unwrap();
    }
    let locator = session.release().await.unwrap();
    assert!(matches!(
        sessions.prune_history(locator).await.unwrap(),
        PruneHistoryOutcome::Pruned(_)
    ));
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("reclaimed:open").unwrap(),
            locator,
            binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("pruned session must reopen");
    };
    let replay = session
        .agent()
        .command(plakous::agent::TurnCommand::Start {
            command_id: command.clone(),
            input: "work".to_owned().into(),
        })
        .await
        .unwrap_or_else(|error| panic!("reclaimed replay: {}", error.message()));
    assert_eq!(
        replay
            .acknowledgement()
            .rejection()
            .map(plakous::agent::ControlError::kind),
        Some(plakous::agent::ControlErrorKind::Reclaimed)
    );
    let attached = AttachedCodingSession {
        output_bound_rederived: None,
        reachability_probe: None,
        sessions,
        locator: session.locator(),
        opened: Some(session),
        recovered_turn: None,
        topology_owner: None,
    };
    let startup = runtime.reserve_startup(&child).await.unwrap();
    let Err(ChildStartFailure::Failed(message)) = start_child_turn_owned_with_ack(
        &runtime,
        &child,
        attached,
        command.clone(),
        "work",
        &startup,
        true,
        StartAdmission::Ordinary,
        std::future::ready(()),
    )
    .await
    else {
        panic!("a reclaimed command starts nothing");
    };
    assert_eq!(message, CHILD_COMMAND_CONSUMED);
    runtime
        .read(|tree| {
            let intent = tree.command(command.as_str()).unwrap();
            assert_eq!(intent.fact, Some(crate::agent_tree::CommandFact::Accepted));
            assert!(!intent.withdrawn);
            assert!(intent.turn_id.is_none());
            assert!(tree.attempt(&child).unwrap().turn_id.is_none());
            assert_eq!(tree.attempt(&child).unwrap().usage.turns, 0);
            assert!(!tree.node(&child).unwrap().active_turn);
        })
        .unwrap();
}

async fn child_start_control_fixture(
    duplicate: bool,
    terminal_before_ack: bool,
    below_retention: bool,
) {
    use crate::agent_tree_protocol::{AttemptStatus, NodeStatus, SpawnAgentRequest, Tier};
    use plakous::model::ModelErrorKind;
    use plakous::sessions::{AgentSessions, SessionBackend};
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let id = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &SpawnAgentRequest::test_named("implement", Tier::Mutate, "implement", "work"),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    let evidence = root.path().join("start-control-evidence.jsonl");
    let release = root.path().join("release-provider");
    let controller = BoundaryController::quiet_gated_from_evidence_path(
        CrashBoundary::ProviderDispatchOne,
        &evidence,
        &release,
    )
    .unwrap();
    let sessions = Arc::new(
        AgentSessions::new(
            SessionBackend::sqlite(root.path().join("start-control.sqlite3")).unwrap(),
        )
        .unwrap(),
    );
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("start-control-create").unwrap(),
            terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                .unwrap(),
        ))
        .await
        .unwrap()
    else {
        panic!("created session required")
    };
    let agent = session.agent().clone();
    let command = CommandId::new("start-control-command").unwrap();
    super::super::result_context::prepare(runtime.driver(), &id, command.as_str(), "work").unwrap();
    if duplicate {
        let first = agent.start(command.clone(), "work").await.unwrap();
        assert!(first.turn().is_some());
    }
    let attached = AttachedCodingSession {
        output_bound_rederived: None,
        reachability_probe: None,
        sessions,
        locator: session.locator(),
        opened: Some(session),
        recovered_turn: None,
        topology_owner: None,
    };
    let startup = runtime.reserve_startup(&id).await.unwrap();
    let started = start_child_turn_owned_with_ack(
        &runtime,
        &id,
        attached,
        command.clone(),
        "work",
        &startup,
        true,
        StartAdmission::Ordinary,
        async {
            tokio::time::timeout(Duration::from_secs(20), async {
                while !plakous_offline_crash_harness::evidence::read_evidence(&evidence)
                    .is_ok_and(|rows| {
                        rows.iter()
                            .any(|row| row.boundary == CrashBoundary::ProviderDispatchOne)
                    })
                {
                    tokio::task::yield_now().await;
                }
                if terminal_before_ack {
                    let replay = agent.start(command.clone(), "work").await.unwrap();
                    assert_eq!(
                        replay.acknowledgement().disposition(),
                        plakous::agent::CommandAckDisposition::Duplicate
                    );
                    assert!(replay.turn().is_none());
                    let result = replay.result().unwrap().clone();
                    let turn = result.turn_id();
                    std::fs::write(&release, "release").unwrap();
                    assert!(matches!(
                        result.result().await.unwrap().outcome(),
                        TurnResultOutcome::Failed(_)
                    ));
                    while agent.turn_control(turn).is_some() {
                        tokio::task::yield_now().await;
                    }
                    if below_retention {
                        let connection = rusqlite::Connection::open(root.path().join("start-control.sqlite3")).unwrap();
                        connection.execute_batch(
                            "DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
                             UPDATE sessions SET feed_retained_from=feed_tail,\
                             feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);",
                        ).unwrap();
                    }
                }
            })
            .await
            .unwrap();
        },
    )
    .await;
    if below_retention {
        let Err(ChildStartFailure::Failed(message)) = started else {
            panic!("a duplicate below retention starts nothing")
        };
        assert!(
            message.contains("duplicate child start has no live turn"),
            "a retained view, not the acknowledgement sequence, settles the duplicate: {message}"
        );
        runtime
            .read(|tree| {
                let intent = tree.command(command.as_str()).unwrap();
                assert_eq!(intent.fact, Some(crate::agent_tree::CommandFact::Accepted));
                assert!(!intent.withdrawn);
                // The startup's initial feed pass already observed the
                // original admission, before the fixture pruned its feed.
                assert!(intent.turn_id.is_some());
                assert!(tree.attempt(&id).unwrap().turn_id.is_none());
                assert_eq!(tree.attempt(&id).unwrap().usage.turns, 1);
                assert!(!tree.node(&id).unwrap().active_turn);
            })
            .unwrap();
    } else if duplicate && terminal_before_ack {
        let Err(ChildStartFailure::Failed(message)) = started else {
            panic!("retired duplicate must require reconciliation")
        };
        assert!(
            message.contains("duplicate child start has no live turn"),
            "{message}"
        );
        super::super::result_context::prepare(runtime.driver(), &id, command.as_str(), "work")
            .unwrap();
        runtime
            .read(|tree| {
                let intent = tree.command(command.as_str()).unwrap();
                assert_eq!(intent.fact, Some(crate::agent_tree::CommandFact::Accepted));
                assert!(intent.turn_id.is_some());
                assert!(!tree.node(&id).unwrap().active_turn);
                assert!(tree.attempt(&id).unwrap().turn_id.is_none());
                assert!(!tree.has_turn_owner(&id));
            })
            .unwrap();
    } else {
        let turn = started.unwrap_or_else(|failure| panic!("{}", String::from(failure)));
        assert_eq!(
            runtime
                .read(|tree| tree.command(command.as_str()).unwrap().turn_id.clone())
                .unwrap(),
            Some(turn.to_string())
        );
        if !terminal_before_ack {
            std::fs::write(&release, "release").unwrap();
        }
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        while runtime.live.lock().await.contains_key(&id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&evidence)
            .unwrap()
            .lines()
            .filter(
                |line| serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                    == "provider_dispatched"
            )
            .count(),
        1,
        "exact command replay cannot redispatch"
    );
    if !duplicate || !terminal_before_ack {
        runtime
            .read(|tree| {
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Failed);
                assert!(!tree.node(&id).unwrap().active_turn);
                assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Failed);
                assert_eq!(
                    tree.attempt(&id).unwrap().failure.as_ref().unwrap().kind,
                    "turn_failed"
                );
                assert_eq!(tree.pending_result_mail("root").len(), 1);
            })
            .unwrap();
    }
}

#[tokio::test]
async fn real_session_late_start_ack_releases_after_deadline_without_redispatch() {
    use crate::agent_tree_protocol::{AttemptScopedRequest, NodeStatus, SpawnAgentRequest, Tier};
    use plakous::model::ModelErrorKind;
    use plakous::sessions::{AgentSessions, SessionBackend};
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let id = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &SpawnAgentRequest::test_named("implement", Tier::Mutate, "implement", "work"),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    let evidence = root.path().join("startup-evidence.jsonl");
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
    let binding = || {
        terminal_failure_runtime_binding(root.path(), controller.clone(), ModelErrorKind::Protocol)
            .unwrap()
    };
    let sessions = Arc::new(
        AgentSessions::new(SessionBackend::sqlite(root.path().join("startup.sqlite3")).unwrap())
            .unwrap(),
    );
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("startup-real-create").unwrap(),
            binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("created session required")
    };
    let original_locator = session.locator();
    let attached = AttachedCodingSession {
        output_bound_rederived: None,
        reachability_probe: None,
        sessions: sessions.clone(),
        opened: Some(session),
        recovered_turn: None,
        locator: original_locator,
        topology_owner: None,
    };
    let startup = runtime.reserve_startup(&id).await.unwrap();
    let caller = StartupCaller::new(runtime.clone(), id.clone(), startup.clone());
    let entered = Arc::new(tokio::sync::Notify::new());
    let deliver_ack = Arc::new(tokio::sync::Notify::new());
    let worker_runtime = runtime.clone();
    let worker_id = id.clone();
    let gate_entered = entered.clone();
    let gate_release = deliver_ack.clone();
    let worker = tokio::spawn(async move {
        start_child_turn_owned_with_ack(
            &worker_runtime,
            &worker_id,
            attached,
            CommandId::new("startup-real-command").unwrap(),
            "run exactly once",
            &startup,
            true,
            StartAdmission::Ordinary,
            async move {
                gate_entered.notify_one();
                gate_release.notified().await;
            },
        )
        .await
    });
    entered.notified().await;
    let dispatches = || {
        std::fs::read_to_string(&evidence)
            .unwrap()
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                    == "provider_dispatched"
            })
            .count()
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while dispatches() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The actual command was admitted and dispatched; only acknowledgement
    // delivery is withheld from the host startup owner.
    assert_eq!(dispatches(), 1);
    let refused = await_child_start(caller, worker, tokio::time::Instant::now())
        .await
        .unwrap_err();
    assert!(String::from(refused).contains("deadline"));
    runtime
        .mutate_then_emit(false, |tree| {
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
            assert!(tree.has_turn_owner(&id));
            assert!(!tree.node(&id).unwrap().active_turn);
            assert!(tree.attempt(&id).unwrap().turn_id.is_none());
            assert!(tree.attempt(&id).unwrap().candidate_id.is_none());
            assert_eq!(tree.revision, tree.stored_revision);
            tree.close_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: id.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap();
            assert!(tree.attempt(&id).unwrap().candidate_id.is_none());
            Ok(())
        })
        .unwrap();
    assert!(runtime.live.lock().await.contains_key(&id));
    deliver_ack.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        while runtime.live.lock().await.contains_key(&id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let released = runtime
        .read(|tree| {
            assert!(!tree.node(&id).unwrap().active_turn);
            assert!(tree.attempt(&id).unwrap().turn_id.is_none());
            assert_eq!(tree.attempt(&id).unwrap().usage.turns, 1);
            assert!(tree.attempt(&id).unwrap().candidate_id.is_none());
            assert_eq!(tree.revision, tree.stored_revision);
            tree.node(&id).unwrap().session_locator.clone().unwrap()
        })
        .unwrap();
    assert_ne!(released, original_locator.to_string());
    let opened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("startup-real-reopen").unwrap(),
            SessionLocator::from_str(&released).unwrap(),
            binding(),
        ))
        .await
        .unwrap();
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        panic!("released child must reopen without an active turn")
    };
    session.release().await.unwrap();
    assert_eq!(dispatches(), 1, "reopen never dispatches a replacement");
}

/// Moves the store's retained-checkpoint counter to where ordinary work
/// stops. The store is already open, so the counter is read as recorded.
fn exhaust_ordinary_checkpoints(database: &Path) {
    use plakous::sessions::{SessionBackend, SessionStoreBound};

    let usage = SessionBackend::inspect_sqlite_usage(database)
        .unwrap()
        .unwrap();
    let checkpoints = usage
        .bounds()
        .iter()
        .find(|bound| bound.bound() == SessionStoreBound::Checkpoints)
        .unwrap();
    rusqlite::Connection::open(database)
        .unwrap()
        .execute(
            "UPDATE store_meta SET checkpoints=?1 WHERE id=1",
            [i64::try_from(checkpoints.ordinary_threshold()).unwrap()],
        )
        .unwrap();
}

fn retained_sessions(database: &Path) -> i64 {
    rusqlite::Connection::open(database)
        .unwrap()
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .unwrap()
}

/// A child-session create refused for store capacity creates nothing, so
/// the spawn is a definite refusal rather than a recovery-required child.
#[tokio::test]
async fn a_child_create_refused_under_storage_pressure_is_a_definite_spawn_refusal() {
    use plakous::model::ModelErrorKind;
    use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionBackend};
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

    let root = tempfile::tempdir().unwrap();
    let evidence = root.path().join("create-pressure-evidence.jsonl");
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
    let database = root.path().join("create-pressure.sqlite3");
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap();
    exhaust_ordinary_checkpoints(&database);
    let Err(error) = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create-under-pressure").unwrap(),
            terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                .unwrap(),
        ))
        .await
    else {
        panic!("a create was admitted without ordinary headroom");
    };
    assert_eq!(error.kind(), AgentSessionsErrorKind::StoragePressure);
    assert_eq!(
        ChildReopenError::from_sessions("recovery create", &error).kind,
        ChildReopenErrorKind::StorageUnavailable,
        "recovery must surface capacity as storage unavailable"
    );
    let SpawnBindError::Refused(message) = child_create_failure(&error, false) else {
        panic!("a capacity-refused create was left for recovery");
    };
    assert!(message.contains("session store is full"), "{message}");
    assert_eq!(
        retained_sessions(&database),
        0,
        "a child session was created"
    );
}

/// A create refused for a held lease is transient, like storage
/// unavailability: even when the accepted-create lookup finds no session
/// it does not prove the spawn never produced one, so the spawn stays
/// recovery-required rather than a definite `spawn_refused`, and recovery
/// keeps it retryable.
#[tokio::test]
async fn a_child_create_refused_for_a_held_lease_is_not_a_definite_spawn_refusal() {
    use plakous::model::ModelErrorKind;
    use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionBackend};
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

    let root = tempfile::tempdir().unwrap();
    let evidence = root.path().join("create-lease-evidence.jsonl");
    let binding = || {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol).unwrap()
    };
    let sessions = AgentSessions::new(
        SessionBackend::sqlite(root.path().join("create-lease.sqlite3")).unwrap(),
    )
    .unwrap();
    let operation = SessionOperationId::new("create-under-held-lease").unwrap();
    let Ok(CreateSessionOutcome::Created { session, .. }) = sessions
        .create(CreateSessionRequest::new(operation.clone(), binding()))
        .await
    else {
        panic!("the first create creates its session");
    };
    let Err(error) = sessions
        .create(CreateSessionRequest::new(operation, binding()))
        .await
    else {
        panic!("a create was admitted while its operation is still attached");
    };
    assert_eq!(error.kind(), AgentSessionsErrorKind::Ownership);
    assert!(
        matches!(
            child_create_failure(&error, true),
            SpawnBindError::RecoveryRequired { .. }
        ),
        "a lease refusal settled the spawn as refused"
    );
    assert!(
        !ChildReopenError::from_sessions("recovery create", &error)
            .kind
            .settles_never_created()
    );
    session.release().await.unwrap();
}

async fn child_start_refusal_under_pressure(delete_fresh_session: bool, cancel_at_ack: bool) {
    use crate::agent_tree_protocol::{NodeStatus, SpawnAgentRequest, Tier};
    use plakous::model::ModelErrorKind;
    use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionBackend};
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let id = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &SpawnAgentRequest::test_named("implement", Tier::Mutate, "implement", "work"),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    let evidence = root.path().join("start-pressure-evidence.jsonl");
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
    let database = root.path().join("start-pressure.sqlite3");
    let sessions =
        Arc::new(AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap());
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("start-pressure-create").unwrap(),
            terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                .unwrap(),
        ))
        .await
        .unwrap()
    else {
        panic!("created session required")
    };
    let locator = session.locator();
    assert_eq!(retained_sessions(&database), 1);
    exhaust_ordinary_checkpoints(&database);
    let attached = AttachedCodingSession {
        output_bound_rederived: None,
        reachability_probe: None,
        sessions: sessions.clone(),
        opened: Some(session),
        recovered_turn: None,
        locator,
        topology_owner: None,
    };
    let startup = runtime.reserve_startup(&id).await.unwrap();
    let cancellation = startup.clone();
    let Err(failure) = start_child_turn_owned_with_ack(
        &runtime,
        &id,
        attached,
        CommandId::new("start-under-pressure").unwrap(),
        "never runs",
        &startup,
        delete_fresh_session,
        StartAdmission::Ordinary,
        async move {
            if cancel_at_ack {
                cancellation.request_cancel();
            }
        },
    )
    .await
    else {
        panic!("a child start was admitted without ordinary headroom");
    };
    assert_eq!(
        matches!(failure, ChildStartFailure::Refused(_)),
        !cancel_at_ack,
        "late cancellation must retain uncertain genesis instead of claiming definite cleanup"
    );
    assert!(
        !std::fs::read_to_string(&evidence)
            .unwrap_or_default()
            .contains("provider_dispatched"),
        "a refused start reached the provider"
    );
    assert_eq!(
        retained_sessions(&database),
        i64::from(!delete_fresh_session || cancel_at_ack),
        "child-session retention disagreed with startup ownership"
    );
    if delete_fresh_session && !cancel_at_ack {
        assert_eq!(
            sessions.archive(locator).await.unwrap_err().kind(),
            AgentSessionsErrorKind::NotFound
        );
    } else {
        let archived = sessions.archive(locator).await.unwrap();
        sessions
            .delete(archived.session_id(), archived.archived_revision())
            .await
            .unwrap();
    }
    runtime
        .read(|tree| {
            assert_ne!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
            assert!(!tree.node(&id).unwrap().active_turn);
        })
        .unwrap();
}

/// A fresh spawn owns its never-started session and may delete it after a
/// definite capacity refusal.
#[tokio::test]
async fn a_child_start_refused_under_storage_pressure_is_definite_and_leaves_no_session() {
    child_start_refusal_under_pressure(true, false).await;
}

/// A mail-delivered Start is reusing an existing child. Its definite
/// refusal rolls the attempt back without deleting that child's history.
#[tokio::test]
async fn an_existing_child_start_refusal_preserves_its_session() {
    child_start_refusal_under_pressure(false, false).await;
}

/// Cancellation can arrive after the rejection acknowledgement but before
/// release. It wins the atomic cleanup claim, so the fresh genesis is kept
/// for explicit later GC rather than being archived underneath the caller.
#[tokio::test]
async fn cancellation_at_cleanup_retains_uncertain_fresh_genesis() {
    child_start_refusal_under_pressure(true, true).await;
}

#[tokio::test]
async fn aborted_startup_worker_frees_its_unpolled_reservation() {
    use crate::agent_tree_protocol::NodeStatus;

    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let startup = runtime.reserve_startup("root").await.unwrap();
    let worker = StartupWorker::new(runtime.clone(), "root".into(), startup);
    let notified = Arc::new(tokio::sync::Notify::new());
    let trigger = notified.clone();
    runtime
        .set_retirement_recovery(Arc::new(move || trigger.notify_one()))
        .await;
    let task = tokio::spawn(async move {
        let _worker = worker;
        std::future::pending::<()>().await;
    });
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), notified.notified())
        .await
        .unwrap();
    assert!(!runtime.live.lock().await.contains_key("root"));
    runtime
        .read(|tree| {
            assert_eq!(
                tree.node("root").unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert_eq!(
                tree.fault_reason("root"),
                Some("child startup owner ended without finishing")
            );
        })
        .unwrap();
}

#[tokio::test]
async fn startup_worker_fallback_does_not_fault_a_transferred_owner() {
    use crate::agent_tree_protocol::NodeStatus;

    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let startup = runtime.reserve_startup("root").await.unwrap();
    let mut worker = StartupWorker::new(runtime.clone(), "root".into(), startup.clone());
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    finish.notify_one();
    assert!(
        runtime
            .occupy_started_child(
                "root".into(),
                &startup,
                Box::new(EndingChild {
                    turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    entered,
                    finish,
                    failure: false,
                })
            )
            .await
            .is_ok()
    );
    worker.finish().await;
    runtime
        .read(|tree| {
            assert_eq!(tree.node("root").unwrap().status, NodeStatus::Running);
            assert!(tree.fault_reason("root").is_none());
        })
        .unwrap();
    crate::topology_runtime::retire_live(runtime.driver(), &runtime.live, "root", None, false)
        .await
        .unwrap();
}

#[tokio::test]
async fn abandoned_startup_caller_cancels_late_registered_turn_once() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let startup = runtime.reserve_startup("root").await.unwrap();
    let caller = StartupCaller::new(runtime.clone(), "root".into(), startup.clone());
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    assert!(
        runtime
            .occupy_started_child(
                "root".into(),
                &startup,
                Box::new(CountedCancellation {
                    child: EndingChild {
                        turn,
                        entered: entered.clone(),
                        finish: finish.clone(),
                        failure: false
                    },
                    calls: calls.clone(),
                })
            )
            .await
            .is_ok()
    );
    // Registration won just before the caller's deadline. Cancellation
    // must target the transferred live owner, not an empty reservation.
    drop(caller);
    entered.notified().await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(runtime.live.lock().await.contains_key("root"));
    finish.notify_one();
    runtime.cancel_live("root").await.unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(runtime.live.lock().await.is_empty());
}

#[tokio::test]
async fn cancelled_startup_rejects_late_ack_owner_without_dropping_it() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let startup = runtime.reserve_startup("root").await.unwrap();
    let caller = StartupCaller::new(runtime.clone(), "root".into(), startup.clone());
    drop(caller);
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rejected = runtime
        .occupy_started_child(
            "root".into(),
            &startup,
            Box::new(CountedCancellation {
                child: EndingChild {
                    turn,
                    entered: entered.clone(),
                    finish: finish.clone(),
                    failure: false,
                },
                calls: calls.clone(),
            }),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(rejected.turn_id(), turn);
    let release_driver = runtime.driver().clone();
    let release = tokio::spawn(async move { rejected.cancel_and_release(release_driver).await });
    entered.notified().await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    finish.notify_one();
    runtime
        .finish_startup("root", &startup, release.await.unwrap())
        .await;
    assert!(runtime.live.lock().await.is_empty());
}

#[tokio::test]
async fn completion_reservation_survives_concurrent_cancel_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let child = || {
        Box::new(EndingChild {
            turn,
            entered: entered.clone(),
            finish: finish.clone(),
            failure: false,
        }) as Box<dyn LiveChildHandle>
    };
    assert!(
        runtime
            .try_occupy_live("root".into(), child())
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let retired = tokio::spawn(async move {
        retire_live_child(owner.driver(), &owner.live, "root", turn).await;
    });
    entered.notified().await;
    assert!(runtime.live.try_lock().is_ok());
    let mut cancel = Box::pin(runtime.cancel_live("root"));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(cancel.as_mut().poll(cx).is_pending()))
            .await
    );
    assert!(
        runtime
            .try_occupy_live("root".into(), child())
            .await
            .is_err()
    );
    finish.notify_one();
    retired.await.unwrap();
    assert_eq!(
        cancel.await.unwrap(),
        crate::topology_runtime::LiveCancelOutcome::Cancelled
    );
    assert!(runtime.live.lock().await.is_empty());
    runtime
        .read(|tree| {
            assert_eq!(tree.node("root").unwrap().session_locator.as_deref(), None);
            assert_eq!(tree.revision, tree.stored_revision);
        })
        .unwrap();
}
#[tokio::test]
async fn failed_normal_release_clears_slot_and_fails_joined_close_canceller() {
    use crate::agent_tree_tools::LiveTurnCanceller;
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let child_id = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    assert!(
        runtime
            .try_occupy_live(
                child_id.clone(),
                Box::new(EndingChild {
                    turn,
                    entered: entered.clone(),
                    finish: finish.clone(),
                    failure: true,
                })
            )
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let id = child_id.clone();
    let retired = tokio::spawn(async move {
        retire_live_child(owner.driver(), &owner.live, &id, turn).await;
    });
    entered.notified().await;
    let canceller = ProductLiveCanceller {
        runtime: runtime.clone(),
    };
    let mut close = canceller.cancel_agents(vec![child_id.clone()]);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(close.as_mut().poll(cx).is_pending()))
            .await
    );
    retired.abort();
    assert!(retired.await.unwrap_err().is_cancelled());
    finish.notify_one();
    let failure = close.await.unwrap_err();
    assert_eq!(failure.failed_ids, vec![child_id.clone()]);
    assert_eq!(failure.not_live_ids, [] as [String; 0]);
    assert_eq!(failure.message, "release refused");
    assert_eq!(
        runtime.cancel_live(&child_id).await.unwrap(),
        crate::topology_runtime::LiveCancelOutcome::NotLive
    );
    assert!(!runtime.live.lock().await.contains_key(&child_id));
    runtime
        .read(|tree| {
            assert_eq!(tree.fault_reason(&child_id), Some("release refused"));
            assert_eq!(tree.revision, tree.stored_revision);
            assert!(tree.node(&child_id).unwrap().session_locator.is_none());
        })
        .unwrap();
}
struct CountedCancellation {
    child: EndingChild,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl LiveChildHandle for CountedCancellation {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        None
    }
    fn turn_id(&self) -> TurnId {
        self.child.turn
    }
    fn turn_control(&self) -> TurnControl {
        panic!("fixture")
    }
    fn follow_up(
        &self,
        _: CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Err("fixture".into()) })
    }
    fn restart(
        &mut self,
        _: TreeDriver,
        _: CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
        Box::pin(async { Err("fixture".into()) })
    }
    fn release(
        self: Box<Self>,
        driver: TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        Box::new(self.child).release(driver)
    }
    fn cancel_and_release(
        self: Box<Self>,
        driver: TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::new(self.child).release(driver)
    }
}

#[tokio::test]
async fn exact_operation_replay_never_claims_pending_cancel_or_cancels_replacement() {
    use plakous::tools::ToolOutcomeKind;
    use std::sync::atomic::{AtomicUsize, Ordering};
    for name in ["interrupt_agent", "close_agent"] {
        for (failed_release, abandon_caller) in [(false, false), (true, false), (false, true)] {
            let root = tempfile::tempdir().unwrap();
            let runtime = runtime(root.path());
            let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
            let child_id = runtime
                .mutate_then_emit(false, |tree| {
                    let child = tree
                        .spawn_agent(
                            "root",
                            &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                            None,
                        )
                        .unwrap()
                        .agent_id;
                    tree.set_active_turn(&child, true).unwrap();
                    tree.bind_attempt_turn(&child, turn.to_string()).unwrap();
                    Ok(child)
                })
                .unwrap();
            let entered = Arc::new(tokio::sync::Notify::new());
            let finish = Arc::new(tokio::sync::Notify::new());
            let calls = Arc::new(AtomicUsize::new(0));
            assert!(
                runtime
                    .try_occupy_live(
                        child_id.clone(),
                        Box::new(CountedCancellation {
                            child: EndingChild {
                                turn,
                                entered: entered.clone(),
                                finish: finish.clone(),
                                failure: failed_release
                            },
                            calls: calls.clone(),
                        })
                    )
                    .await
                    .is_ok()
            );
            let arguments = serde_json::json!({ "agent_id": child_id });
            let invoke = || {
                crate::agent_tree_tools::invoke_granted_topology_tool_on(
                    runtime.driver().clone(),
                    "root".into(),
                    name,
                    &arguments,
                    "exact-cancel".into(),
                    crate::agent_tree_tools::TopologyHostPorts {
                        canceller: Some(Arc::new(ProductLiveCanceller {
                            runtime: runtime.clone(),
                        })),
                        ..Default::default()
                    },
                )
            };
            let mut original = Box::pin(invoke());
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(
                    original.as_mut().poll(cx).is_pending()
                ))
                .await
            );
            entered.notified().await;
            let pending = invoke().await.unwrap();
            assert_eq!(pending.kind(), ToolOutcomeKind::Failure);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            runtime
                .read(|tree| {
                    let receipt = &tree.operations()["exact-cancel"];
                    assert!(receipt.terminal_response.is_none());
                    assert!(receipt.terminal_code.is_none());
                })
                .unwrap();
            if abandon_caller {
                drop(original);
                finish.notify_one();
                assert!(runtime.cancel_live(&child_id).await.is_ok());
                assert_eq!(invoke().await.unwrap().kind(), ToolOutcomeKind::Failure);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                continue;
            }
            finish.notify_one();
            let expected = if failed_release {
                ToolOutcomeKind::Failure
            } else {
                ToolOutcomeKind::Success
            };
            assert_eq!(original.await.unwrap().kind(), expected);
            assert_eq!(invoke().await.unwrap().kind(), expected);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let replacement = TurnId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
            let occupied = runtime
                .try_occupy_live(
                    child_id.clone(),
                    Box::new(CountedCancellation {
                        child: EndingChild {
                            turn: replacement,
                            entered: entered.clone(),
                            finish: finish.clone(),
                            failure: false,
                        },
                        calls: calls.clone(),
                    }),
                )
                .await;
            assert!(occupied.is_ok(), "returned release no longer owns the slot");
            assert_eq!(invoke().await.unwrap().kind(), expected);
            assert_eq!(
                runtime.live.lock().await.get(&child_id).unwrap().turn_id(),
                replacement
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}
