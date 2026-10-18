//! Tree driver: folds, commits, saves, settles and their refusals.

use super::*;

#[test]
fn cascade_targets_fold_within_commit_and_restore_on_refused_save() {
    for refuse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(if refuse { 3 } else { usize::MAX }),
            stale: false,
        });
        let mut tree = bootstrap_tree("cascade-post-pass", root.path());
        tree.grants_mut_for_test("root").max_depth = 2;
        let parent = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap()
            .agent_id;
        let child = tree
            .spawn_agent(&parent, &spawn_request(), None)
            .unwrap()
            .agent_id;
        let driver = driver_on(tree, store);
        let mut tree = driver.lock().unwrap();
        driver.save(&mut tree).unwrap();
        tree.record_host_cancellation(&parent).unwrap();
        driver.save(&mut tree).unwrap();
        let before = tree.node(&child).unwrap().clone();
        let result = driver.fold_and_commit(&mut tree, |tree| {
            tree.fold_due(&parent)?;
            assert_eq!(tree.node(&child).unwrap(), &before);
            assert!(tree.pending_close_folds.contains(&child));
            Ok(())
        });
        assert_eq!(result.is_err(), refuse);
        assert!(tree.pending_close_folds.is_empty());
        if refuse {
            assert_eq!(tree.node(&child).unwrap(), &before);
            assert!(tree.take_effects().is_empty());
        } else {
            assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
        }
    }
}

#[test]
fn refused_startup_consequence_does_not_install_or_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(SecondSaveStore {
        inner: MemoryTreeStore::new(),
        saves: AtomicUsize::new(0),
        fail_at: AtomicUsize::new(3),
        stale: false,
    });
    let mut tree = bootstrap_tree("startup-save-refusal", root.path());
    let child = tree
        .spawn_agent("root", &spawn_request(), None)
        .unwrap()
        .agent_id;
    tree.record_host_cancellation(&child).unwrap();
    tree.acknowledge_revision(store.save(&tree).unwrap());
    let id = tree.tree_id.clone();
    let loaded = ProductAgentTree::reopen(store.load(&id).unwrap().unwrap());
    let hub = crate::hub::TopologyHub::new();
    assert!(
        hub.insert_root(&id, loaded, store.clone(), tree.vcs_port(), None)
            .is_err()
    );
    assert!(hub.lookup(&id).is_none());
    let stored = store.load(&id).unwrap().unwrap();
    assert_eq!(
        stored.rows().node(&child).unwrap().status,
        NodeStatus::Running
    );
    assert!(
        stored
            .rows()
            .operations()
            .values()
            .filter(|operation| operation.fold.is_some())
            .all(|operation| operation.terminal_response.is_none())
    );
}

#[test]
fn scratch_and_refused_folds_do_not_publish_workspace_generation() {
    use plakous_workspace::occupancy_vcs::fake::FakeVcs;
    for scratch in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "generation-boundary",
            "root",
            "owner",
            root.path().to_owned(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap()
            .agent_id;
        let counter = tree
            .generation_counter_for_root(&tree.node(&child).unwrap().path)
            .unwrap();
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(2),
            stale: false,
        });
        let driver = driver_on(tree, store);
        let mut tree = driver.lock().unwrap();
        driver.save(&mut tree).unwrap();
        if scratch {
            tree.clone().record_turn_completed(&child).unwrap();
        } else {
            assert!(
                driver
                    .fold_and_commit(&mut tree, |tree| tree.record_turn_completed(&child))
                    .is_err()
            );
        }
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(
            tree.workspace(&tree.node(&child).unwrap().workspace_id)
                .unwrap()
                .generation,
            0
        );
        if !scratch {
            driver
                .fold_and_commit(&mut tree, |tree| tree.record_turn_completed(&child))
                .unwrap();
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn child_retention_runtime_archive_delete_crash_cuts() {
    use plakous::sessions::*;
    use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
    for cut in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let controller = BoundaryController::quiet_from_evidence_path(
            plakous_offline_crash_harness::protocol::CrashBoundary::None,
            root.path().join("evidence.jsonl"),
        )
        .unwrap();
        let plan = Arc::new(
            serde_json::from_value(
                json!({"version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "steps": [{"output": {"text": "done"}}]}),
            )
            .unwrap(),
        );
        let runtime = scripted_runtime_binding(root.path(), controller, plan).unwrap();
        let sessions = AgentSessions::new(
            SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
        )
        .unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("retention:create").unwrap(),
                runtime,
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        let locator = session.release().await.unwrap();
        let mut tree = bootstrap_tree("child-retention-runtime", root.path());
        let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.bind_session_locator(&child.agent_id, locator.to_string())
            .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let result = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        tree.acknowledge_wait_mail("root", &result);
        assert_eq!(tree.prepare_child_retention(u64::MAX).unwrap().len(), 1);
        let store = Arc::new(MemoryTreeStore::new());
        let driver = driver_on(tree, store.clone());
        driver.mutate(|tree| driver.save(tree)).unwrap().0.unwrap();
        if cut > 0 {
            sessions.archive(locator).await.unwrap();
        }
        if cut > 1 {
            sessions
                .delete_archived(locator.session_id())
                .await
                .unwrap();
        }
        driver.collect_dormant_children(&sessions).await.unwrap();
        driver.collect_dormant_children(&sessions).await.unwrap();
        assert!(
            driver
                .read(|tree| tree.node(&child.agent_id).is_err())
                .unwrap()
        );
        assert!(
            sessions
                .delete_archived(locator.session_id())
                .await
                .unwrap()
                .is_none()
        );
    }
}

/// A store that refuses every publish, for the paths that must still
/// observe and report when persistence is gone.
#[tokio::test]
async fn cancellation_prepare_publish_refusal_changes_no_node_and_dispatches_nothing() {
    for close in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("refused-cancel-prepare", root.path());
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.set_active_turn("c1", true).unwrap();
        let driver = driver_on(tree, Arc::new(FailingStore));
        let canceller = Arc::new(StubCanceller::ok());
        let tool = if close {
            close_tool(driver.clone(), Some(canceller.clone()))
        } else {
            interrupt_tool(driver.clone(), Some(canceller.clone()))
        };
        let outcome = if close {
            tool.close_with("refused-prepare".into(), &agent_id_request("c1"))
                .await
        } else {
            tool.interrupt_with("refused-prepare".into(), &agent_id_request("c1"))
                .await
        }
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert!(canceller.calls.lock().unwrap().is_empty());
        driver
            .read(|tree| {
                assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
                assert!(tree.node("c1").unwrap().active_turn);
                assert!(
                    tree.operations()["refused-prepare"]
                        .terminal_response
                        .is_none()
                );
            })
            .unwrap();
    }
}

struct SecondSaveStore {
    inner: MemoryTreeStore,
    saves: AtomicUsize,
    fail_at: AtomicUsize,
    stale: bool,
}

#[test]
fn deleted_fold_stays_deleted_without_a_live_refold_callback() {
    for refused in [false, true] {
        for next_entry in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(if refused { 2 } else { 0 }),
                stale: false,
            });
            let mut tree = bootstrap_tree("refold-ack", tmp.path());
            let child = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .unwrap();
            tree.record_child_session(&child.agent_id,
                "00000000-0000-4000-8000-000000000100/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001").unwrap();
            tree.acknowledge_revision(store.save(&tree).unwrap());
            let driver = driver_on(tree, store.clone());
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let shared = driver.tree.clone();
            driver.set_retirement_recovery(Arc::new(move || {
                assert!(
                    shared.try_lock().is_ok(),
                    "callback must run outside the tree mutex"
                );
                observed.fetch_add(1, Ordering::SeqCst);
            }));
            let (result, effects) = driver
                .mutate(|tree| {
                    driver.fold_and_commit(tree, |tree| {
                        tree.clear_feed_views();
                        Ok(())
                    })
                })
                .unwrap();
            assert_eq!(result.is_err(), refused);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            // Crash after the clearing save and before any process-local callback.
            let loaded = store.load("refold-ack").unwrap().unwrap();
            assert_eq!(
                loaded
                    .rows()
                    .node(&child.agent_id)
                    .unwrap()
                    .fold_order
                    .is_none(),
                !refused
            );
            if !refused {
                let reopened = ProductAgentTree::reopen(loaded);
                assert!(
                    reopened
                        .session_recovery_targets()
                        .contains(&child.agent_id)
                );
            }
            if next_entry {
                driver.read(|_| ()).unwrap();
            }
            driver.settle(effects).unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            driver.read(|_| ()).unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn tool_consequence_second_save_refusal_restores_views_and_dispatches_nothing() {
    use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
    for stale in [false, true] {
        for kind in [
            TopologyKind::Close,
            TopologyKind::Interrupt,
            TopologyKind::Submit,
            TopologyKind::Discard,
        ] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(0),
                stale,
            });
            let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "tool-second-save",
                "root",
                "owner",
                root.path().to_owned(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
            binding.max_tier = Tier::Mutate;
            tree.pin_template("implement", binding);
            let child = tree
                .spawn_agent("root", &mint_spawn_request(), None)
                .unwrap()
                .agent_id;
            if kind == TopologyKind::Interrupt {
                tree.set_active_turn(&child, true).unwrap();
            }
            if kind == TopologyKind::Discard {
                tree.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: child.clone(),
                        expected_attempt_id: None,
                    },
                    None,
                )
                .unwrap();
                tree.settle_for_test();
            }
            let before_node = tree.node(&child).unwrap().clone();
            let before_attempt = tree.attempt(&child).unwrap().clone();
            let before_workspaces = tree.workspaces().clone();
            let workspace = tree.node(&child).unwrap().workspace_id.clone();
            let driver = driver_on(tree, store.clone());
            driver.save(&mut driver.lock().unwrap()).unwrap();
            store
                .fail_at
                .store(store.saves.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
            let mut tool = close_tool(driver.clone(), None);
            tool.kind = kind;
            let outcome = match kind {
                TopologyKind::Close => tool
                    .close_with("refused-consequence".into(), &agent_id_request(&child))
                    .await
                    .unwrap(),
                TopologyKind::Interrupt => tool
                    .interrupt_with("refused-consequence".into(), &agent_id_request(&child))
                    .await
                    .unwrap(),
                TopologyKind::Submit => {
                    *tool.caller_id.lock().unwrap() = child.clone();
                    tool.run_named(
                        "refused-consequence",
                        &json!({"turn_token": 1, "output_json": "{\"ok\":true}"}),
                    )
                    .unwrap()
                }
                TopologyKind::Discard => tool
                    .run_named("refused-consequence", &workspace_request(&workspace))
                    .unwrap(),
                _ => unreachable!(),
            };
            assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
            assert_eq!(
                store.saves.load(Ordering::SeqCst),
                store.fail_at.load(Ordering::SeqCst) + 1,
                "the consequence save must fail before the failure receipt is published",
            );
            driver
                .read(|tree| {
                    assert_eq!(tree.node(&child).unwrap(), &before_node);
                    assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
                    assert_eq!(tree.workspaces(), &before_workspaces);
                    assert!(
                        tree.operations()["refused-consequence"]
                            .terminal_code
                            .is_some()
                    );
                    assert!(
                        !tree.operations().values().any(|op| op.tool_name == "close"
                            || op.tool_name == "interrupt"
                            || op.tool_name == "submitted")
                            || kind == TopologyKind::Discard
                    );
                })
                .unwrap();
            assert!(driver.lock().unwrap().take_effects().is_empty());
            let seals_before = usize::from(kind == TopologyKind::Discard);
            assert_eq!(vcs.calls_of(FakeOp::Seal).len(), seals_before);
            assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
        }
    }
}

impl TreeStore for SecondSaveStore {
    fn exists(&self, id: &str) -> Result<bool, crate::tree::TreeStoreError> {
        self.inner.exists(id)
    }

    fn ids(&self) -> Result<Vec<String>, crate::tree::TreeStoreError> {
        self.inner.ids()
    }

    fn load(
        &self,
        id: &str,
    ) -> Result<Option<crate::tree::PersistedTree>, crate::tree::TreeStoreError> {
        self.inner.load(id)
    }

    fn save(
        &self,
        tree: &ProductAgentTree,
    ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
        let save = self.saves.fetch_add(1, Ordering::SeqCst) + 1;
        if save == self.fail_at.load(Ordering::SeqCst) {
            return Err(if self.stale {
                crate::tree::TreeStoreError::stale(tree.stored_revision + 1, tree.revision)
            } else {
                crate::tree::TreeStoreError::unavailable("second save refused")
            });
        }
        self.inner.save(tree)
    }
}

#[test]
fn recorded_intent_second_save_refusal_reverts_and_reserves_once_on_retry() {
    use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
    for stale in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(0),
            stale,
        });
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "second-save",
            "root",
            "owner",
            root.path().to_owned(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap()
            .agent_id;
        let driver = driver_on(tree, store.clone());
        let mut tree = driver.lock().unwrap();
        driver.save(&mut tree).unwrap();
        tree.record_host_cancellation(&child).unwrap();
        let before_node = tree.node(&child).unwrap().clone();
        let before_attempt = tree.attempt(&child).unwrap().clone();
        store
            .fail_at
            .store(store.saves.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
        assert!(driver.apply_recorded(&mut tree, &child).is_err());
        assert_eq!(tree.node(&child).unwrap(), &before_node);
        assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
        assert_eq!(tree.revision, tree.stored_revision);
        assert!(tree.take_effects().is_empty());
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
        driver.apply_recorded(&mut tree, &child).unwrap();
        let effects = tree.take_effects();
        assert!(!effects.is_empty());
        drop(tree);
        driver.settle(effects).unwrap();
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
        let mut tree = driver.lock().unwrap();
        driver.apply_recorded(&mut tree, &child).unwrap();
        assert!(tree.take_effects().is_empty());
    }
}

#[test]
fn runtime_second_save_refusal_reverts_and_dispatches_only_after_retry() {
    use crate::hub::ProductTopologyRuntime;
    use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
    for stale in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(2),
            stale,
        });
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "runtime-second-save",
            "root",
            "owner",
            root.path().to_owned(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap()
            .agent_id;
        tree.reserve_turn_owner(&child);
        tree.release_turn_owner(&child);
        let before_node = tree.node(&child).unwrap().clone();
        let before_attempt = tree.attempt(&child).unwrap().clone();
        let runtime = ProductTopologyRuntime::new(tree, store, vcs.clone(), None);
        let close = |tree: &mut ProductAgentTree| {
            tree.close_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .map_err(|error| error.message)
        };
        assert!(runtime.mutate_then_emit(false, close).is_err());
        runtime
            .read(|tree| {
                assert_eq!(tree.node(&child).unwrap(), &before_node);
                assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
                assert!(
                    !tree.has_turn_owner(&child),
                    "rollback must not resurrect released ownership"
                );
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
        runtime.mutate_then_emit(false, close).unwrap();
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
        runtime.mutate_then_emit(false, |_| Ok(())).unwrap();
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
    }
}

#[test]
fn fail_closed_second_save_refusal_dispatches_nothing_until_retry() {
    use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
    for stale in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(2),
            stale,
        });
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "fail-closed-second-save",
            "root",
            "owner",
            root.path().to_owned(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap()
            .agent_id;
        let before_node = tree.node(&child).unwrap().clone();
        let before_attempt = tree.attempt(&child).unwrap().clone();
        let driver = driver_on(tree, store);
        let tool = task_tool(driver.clone(), TopologyKind::Spawn);
        tool.fail_closed_session(&child, "materialization failed".into())
            .unwrap();
        driver
            .read(|tree| {
                assert_eq!(tree.node(&child).unwrap(), &before_node);
                assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
        tool.fail_closed_session(&child, "materialization failed".into())
            .unwrap();
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
    }
}

#[test]
fn fold_commit_refuses_dirty_input_and_reverts_work_refusal() {
    let root = tempfile::tempdir().unwrap();
    let driver = driver_on(
        bootstrap_tree("fold-commit-refusals", root.path()),
        Arc::new(MemoryTreeStore::new()),
    );
    let mut tree = driver.lock().unwrap();
    assert!(
        driver
            .fold_and_commit::<()>(&mut tree, |_| panic!("dirty input must not run work"))
            .is_err()
    );
    driver.save(&mut tree).unwrap();
    let before = tree.revision;
    let result: Result<(), TopologyError> = driver.fold_and_commit(&mut tree, |tree| {
        tree.record_fault("root", "uncommitted diagnostic");
        Err(TopologyError::new(
            TopologyErrorCode::GrantDenied,
            "work refused",
        ))
    });
    assert_eq!(result.unwrap_err().code, TopologyErrorCode::GrantDenied);
    assert_eq!(tree.revision, before);
    assert_eq!(tree.fault_reason("root"), None);
}

#[test]
fn recorded_runtime_intent_survives_consequence_refusal_without_resurrecting_owner() {
    use crate::hub::ProductTopologyRuntime;
    use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
    for stale in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(2),
            stale,
        });
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "recorded-runtime-refusal",
            "root",
            "owner",
            root.path().to_owned(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap()
            .agent_id;
        tree.reserve_turn_owner(&child);
        let before_node = tree.node(&child).unwrap().clone();
        let before_attempt = tree.attempt(&child).unwrap().clone();
        let runtime = ProductTopologyRuntime::new(tree, store.clone(), vcs.clone(), None);
        assert!(
            runtime
                .record_then_apply(&child, |tree| {
                    tree.release_turn_owner(&child);
                    tree.record_host_cancellation(&child)
                })
                .is_err()
        );
        runtime
            .read(|tree| {
                assert_eq!(tree.node(&child).unwrap(), &before_node);
                assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
                assert!(!tree.has_turn_owner(&child));
                assert_eq!(tree.revision, tree.stored_revision);
                assert_eq!(
                    tree.operations(),
                    store
                        .load(&tree.tree_id)
                        .unwrap()
                        .unwrap()
                        .rows()
                        .operations()
                );
            })
            .unwrap();
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
        runtime
            .record_then_apply(&child, |tree| tree.record_host_cancellation(&child))
            .unwrap();
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
    }
}

#[test]
fn refused_effect_result_save_keeps_follow_on_removal_queued_until_acknowledged() {
    use crate::hub::ProductTopologyRuntime;
    use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
    for stale in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(2),
            stale,
        });
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "follow-on-save-refusal",
            "root",
            "owner",
            root.path().to_owned(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        binding.on_mint = Some("fixture-hook".into());
        tree.pin_template("implement", binding);
        tree.spawn_agent("root", &mint_spawn_request(), None)
            .unwrap();
        let runtime = ProductTopologyRuntime::new(tree, store, vcs.clone(), None);
        let driver = runtime.driver();
        vcs.hook_next_with(Err(VcsError::hook_failed("fixture hook failed")));
        let effects = {
            let mut tree = driver.lock().unwrap();
            driver.save(&mut tree).unwrap();
            tree.take_effects()
        };
        assert!(driver.settle(effects).is_err());
        assert_eq!(vcs.calls_of(FakeOp::Hook).len(), 1);
        assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
        runtime.mutate_then_emit(false, |_| Ok(())).unwrap();
        assert_eq!(vcs.calls_of(FakeOp::Hook).len(), 1);
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
    }
}

#[test]
fn followup_withdrawal_cas_refusal_restores_the_complete_opening() {
    for stale in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("tree1", root.path());
        let spawned = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.record_child_session(&spawned.agent_id,
            "00000000-0000-4000-8000-000000000100/00000000-0000-4000-8000-000000000200/00000000-0000-4000-8000-000000000300").unwrap();
        tree.record_host_cancellation(&spawned.agent_id).unwrap();
        tree.fold_due(&spawned.agent_id).unwrap();
        let opener = tree
            .send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: spawned.agent_id.clone(),
                    message: "again".to_owned(),
                    purpose: Some(MessagePurpose::Followup),
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                Some("withdraw-send"),
            )
            .unwrap()
            .message_id;
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(2),
            stale,
        });
        tree.acknowledge_revision(store.save(&tree).unwrap());
        let before_mail = tree.mail(&opener).unwrap().clone();
        let before_nodes = tree.nodes().clone();
        let before_attempts = tree.attempts().clone();
        let before_operations = tree.operations().clone();
        let revision = tree.revision;
        let driver = driver_on(tree, store.clone());
        assert!(
            driver
                .withdraw_unstarted_followup(
                    &spawned.agent_id,
                    &opener,
                    "00000000-0000-4000-8000-000000000100",
                    0,
                    Some((
                        "withdraw-send".to_owned(),
                        TopologyErrorCode::RecipientAttemptTerminal
                    )),
                )
                .is_err()
        );
        driver
            .read(|tree| {
                assert_eq!(tree.mail(&opener), Some(&before_mail));
                assert_eq!(tree.nodes(), &before_nodes);
                assert_eq!(tree.attempts(), &before_attempts);
                assert_eq!(tree.operations(), &before_operations);
                assert_eq!(tree.revision, revision);
            })
            .unwrap();
        driver
            .withdraw_unstarted_followup(
                &spawned.agent_id,
                &opener,
                "00000000-0000-4000-8000-000000000100",
                0,
                Some((
                    "withdraw-send".to_owned(),
                    TopologyErrorCode::RecipientAttemptTerminal,
                )),
            )
            .unwrap();
        let saved = store.load("tree1").unwrap().unwrap();
        assert_eq!(saved.rows().attempts().len(), before_attempts.len() - 1);
        assert_eq!(
            saved.rows().mail(&opener).unwrap().durable,
            MailDurableState::Acknowledged
        );
        assert_eq!(
            saved.rows().operations()["withdraw-send"].terminal_code,
            Some(TopologyErrorCode::RecipientAttemptTerminal)
        );
        assert_eq!(store.saves.load(Ordering::SeqCst), 3);
    }
}

/// A refused publish does not stop the drain: the seal the port ran is
/// recorded — the node leaves `Sealing`, the candidate is in the tree —
/// and the save error is reported after, so no effect is lost to a
/// stale or absent store.
#[tokio::test]
async fn settle_records_every_effect_before_reporting_a_refused_publish() {
    use crate::protocol::NodeStatus;
    use plakous_workspace::occupancy_vcs::fake::FakeVcs;

    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::clone(&vcs) as Arc<dyn VcsOps>,
        "tree-settle",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    tree.pin_template("implement", binding);
    let store = Arc::new(SecondSaveStore {
        inner: MemoryTreeStore::new(),
        saves: AtomicUsize::new(0),
        fail_at: AtomicUsize::new(0),
        stale: false,
    });
    let driver = driver_on(tree, store.clone());
    let child = driver
        .mutate(|tree| {
            tree.spawn_agent("root", &mint_spawn_request(), None)
                .unwrap()
                .agent_id
        })
        .unwrap()
        .0;
    let (closed, effects) = driver
        .mutate(|tree| {
            tree.close_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
        })
        .unwrap();
    closed.unwrap();
    assert!(!effects.is_empty(), "closing a mutate copy seals it");
    driver.save(&mut driver.lock().unwrap()).unwrap();
    store
        .fail_at
        .store(store.saves.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    let error = driver.settle(effects).unwrap_err();
    assert!(
        error.message.contains("second save refused"),
        "{}",
        error.message
    );
    let mut tree = driver.tree.lock().unwrap();
    assert_ne!(tree.node(&child).unwrap().status, NodeStatus::Sealing);
    assert!(
        tree.attempt(&child).unwrap().candidate_id.is_some(),
        "the seal that ran is recorded"
    );
    assert!(
        tree.take_effects().is_empty(),
        "the drain ran to completion"
    );
}

#[tokio::test]
async fn settle_reports_a_seal_failure_after_recording_and_publishing_it() {
    use crate::protocol::NodeStatus;
    use plakous_workspace::occupancy_vcs::fake::FakeVcs;

    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        "tree-seal-failure",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    tree.pin_template("implement", binding);
    let store = Arc::new(MemoryTreeStore::default());
    let driver = driver_on(tree, store.clone());
    let child = driver
        .mutate_and_settle(|tree| {
            tree.spawn_agent("root", &mint_spawn_request(), None)
                .unwrap()
                .agent_id
        })
        .unwrap()
        .0;
    vcs.seal_next_with(Err(VcsError::seal_failed("injected seal failure")));
    let ((), effects) = driver
        .mutate(|tree| {
            tree.record_turn_completed(&child).unwrap();
            driver.save(tree).unwrap();
        })
        .unwrap();
    let settled = driver.settle(effects).unwrap();
    let error = settled
        .seal_failures
        .first()
        .expect("the seal failure is surfaced");
    assert_eq!(error.code, TopologyErrorCode::MintUnavailable);
    assert!(error.message.contains("injected seal failure"));
    driver
        .read(|tree| {
            assert_eq!(
                tree.node(&child).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert_eq!(tree.revision, tree.stored_revision);
            let reopened = ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
            assert_eq!(
                reopened
                    .attempt(&child)
                    .unwrap()
                    .failure
                    .as_ref()
                    .unwrap()
                    .kind,
                "seal_failed"
            );
        })
        .unwrap();
}

/// The design's headline oracle: no VCS subprocess
/// runs under the tree mutex. A `FakeVcs` watching the tree's own mutex
/// records, per call, whether the lock was held; every mint, occupancy
/// probe, seal, unpin, adopt, hook, and unmint the tools drive must have
/// found it released. There is no exception: the mint is a ticket
/// `spawn_reserve` hands back, and the `list_agents` probes are a
/// snapshot taken before the render.
#[tokio::test]
async fn no_vcs_subprocess_runs_under_the_tree_mutex() {
    use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};

    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::clone(&vcs) as Arc<dyn VcsOps>,
        "tree-lock",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    binding.on_mint = Some("true".into());
    tree.pin_template("implement", binding);
    let driver = driver_on(tree, Arc::new(MemoryTreeStore::new()));
    let tree = Arc::clone(&driver.tree);
    let watched = Arc::clone(&tree);
    vcs.watch_lock(move || matches!(watched.try_lock(), Err(std::sync::TryLockError::WouldBlock)));

    // spawn: mint (under the lock, by design) then the hook (an effect).
    let spawned = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-spawn".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(spawned.kind(), ToolOutcomeKind::Success, "{spawned:?}");
    let child = outcome_json(&spawned)["agent_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let workspace_id = outcome_json(&spawned)["workspace"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // close: the seal (an effect).
    let closed = close_tool(driver.clone(), None)
        .close_with("op-close".into(), &agent_id_request(&child))
        .await
        .unwrap();
    assert_eq!(closed.kind(), ToolOutcomeKind::Success, "{closed:?}");
    let candidate_id = tree
        .lock()
        .unwrap()
        .attempt(&child)
        .unwrap()
        .candidate_id
        .clone()
        .expect("the closed copy sealed");
    // adopt: the landing (an effect).
    let adopted = adopt_tool(driver.clone())
        .adopt_with(
            "op-adopt",
            &serde_json::to_value(AdoptCandidateRequest {
                candidate_id,
                expected_target: None,
                evidence: None,
            })
            .unwrap(),
        )
        .unwrap();
    assert_eq!(adopted.kind(), ToolOutcomeKind::Success, "{adopted:?}");
    // discard: the unmint (an effect).
    let discarded = discard_tool(driver.clone())
        .run_named("op-discard", &workspace_request(&workspace_id))
        .unwrap();
    assert_eq!(discarded.kind(), ToolOutcomeKind::Success, "{discarded:?}");

    // list: the occupancy probes (a snapshot taken before the render).
    let listed = list_tool(driver.clone())
        .run_named(
            "op-list",
            &serde_json::to_value(ListAgentsRequest {
                include_read: None,
                include_self: None,
                include_completed: Some(true),
            })
            .unwrap(),
        )
        .unwrap();
    assert_eq!(listed.kind(), ToolOutcomeKind::Success, "{listed:?}");

    let calls = vcs.calls();
    for op in [
        FakeOp::Mint,
        FakeOp::Hook,
        FakeOp::Seal,
        FakeOp::Adopt,
        FakeOp::Unmint,
        FakeOp::Dirty,
    ] {
        assert!(
            calls.iter().any(|call| call.op == op),
            "{op:?} ran: {calls:?}"
        );
    }
    let under_lock: Vec<_> = calls
        .iter()
        .filter(|call| call.op.is_effect() && call.lock_held)
        .collect();
    assert!(
        under_lock.is_empty(),
        "a VCS subprocess ran under the tree mutex: {under_lock:?}"
    );
    // The store saw every publish the settle made.
    let stored = driver
        .store
        .load("tree-lock")
        .unwrap()
        .expect("the settled tree was published");
    assert_eq!(stored.rows().candidates().len(), 1);
}

/// A mutation cannot strand what it queued.
///
/// `ProductTopologyRuntime.tree` and `TreeDriver.tree` used to be `pub`,
/// so a caller above this crate could lock the tree, mutate it, and drop
/// the guard with the seal that mutation queued still sitting on the
/// tree — the node stays `Sealing` and reopens `RecoveryRequired`, the
/// same fault a crash mid-subprocess leaves. The handle is private now
/// and [`TreeDriver::mutate`] is the way through, which cannot lose the
/// effects because it hands them back: the tree owes nothing the moment
/// the mutation returns.
#[test]
fn a_mutation_through_the_driver_cannot_strand_its_effects() {
    let (root, store, mut tree) = git_persist_fixture();
    let child = tree
        .spawn_agent("root", &mint_spawn_request(), None)
        .unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.settle_for_test();
    let driver = driver_on(tree, Arc::clone(&store));
    assert!(
        !driver.read(ProductAgentTree::owes_effects).unwrap(),
        "the fixture starts settled"
    );

    // A close queues the child's seal.
    let (closed, effects) = driver
        .mutate(|tree| {
            tree.close_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: child.agent_id.clone(),
                    expected_attempt_id: None,
                },
                Some("op-strand"),
            )
        })
        .unwrap();
    assert!(closed.is_ok());
    assert!(!effects.is_empty(), "the close owed the VCS a seal");
    assert!(
        !driver.read(ProductAgentTree::owes_effects).unwrap(),
        "the mutation handed its effects out; the tree kept none"
    );

    // Until they are settled the node is still mid-seal, which is exactly
    // what a dropped batch would have left behind forever.
    assert_eq!(
        driver
            .read(|tree| tree.node(&child.agent_id).unwrap().status)
            .unwrap(),
        NodeStatus::Sealing
    );
    driver.save(&mut driver.lock().unwrap()).unwrap();
    driver.settle(effects).unwrap();
    assert_eq!(
        driver
            .read(|tree| tree.node(&child.agent_id).unwrap().status)
            .unwrap(),
        NodeStatus::Closed
    );
    assert_eq!(
        driver.read(|tree| tree.candidates().len()).unwrap(),
        1,
        "the seal ran and left the candidate"
    );
    drop(root);
}

/// The settled pairing does both halves in one call, so the window in
/// which a batch can be dropped does not exist for its callers.
#[test]
fn mutate_and_settle_leaves_nothing_owed() {
    let (root, store, mut tree) = git_persist_fixture();
    let child = tree
        .spawn_agent("root", &mint_spawn_request(), None)
        .unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.settle_for_test();
    let driver = driver_on(tree, Arc::clone(&store));
    let (closed, _settled) = driver
        .mutate_and_settle(|tree| {
            tree.close_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: child.agent_id.clone(),
                    expected_attempt_id: None,
                },
                Some("op-settled"),
            )
        })
        .unwrap();
    assert!(closed.is_ok());
    assert!(!driver.read(ProductAgentTree::owes_effects).unwrap());
    assert_eq!(
        driver
            .read(|tree| tree.node(&child.agent_id).unwrap().status)
            .unwrap(),
        NodeStatus::Closed
    );
    assert_eq!(driver.read(|tree| tree.candidates().len()).unwrap(), 1);
    drop(root);
}
