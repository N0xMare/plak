//! Workspace copies: minting, discard, seals and adoption.

use super::*;

#[tokio::test]
async fn hub_working_copy_is_shared_across_compiles_and_persists() {
    let root = tempfile::tempdir().unwrap();
    let tree_id = format!("hub-{}", uuid::Uuid::new_v4().simple());
    let mut tree = bootstrap_tree(&tree_id, root.path());
    let store: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
    tree.acknowledge_revision(store.save(&tree).unwrap());
    let runtime =
        crate::hub::insert_root_with(tree_id.clone(), tree, Arc::clone(&store), None).unwrap();
    let parent_tree = Arc::clone(&runtime.tree);
    let child_lookup = crate::hub::lookup(&tree_id).unwrap();
    assert!(Arc::ptr_eq(&parent_tree, &child_lookup.tree));
    let binder = Arc::new(StubBinder::ok());
    let outcome = spawn_tool(
        runtime.driver().clone(),
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-hub-spawn".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let names = granted_names(Arc::clone(&child_lookup.tree), "c1");
    assert!(names.iter().any(|name| name == "submit_result"));
    assert!(!names.iter().any(|name| name == "spawn_agent"));
    let loaded = reload(&store, &tree_id);
    assert_eq!(
        loaded.node("c1").unwrap().session_locator.as_deref(),
        Some(
            "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
        )
    );
    crate::hub::force_release(&tree_id).await;
}

#[tokio::test]
async fn read_only_spawn_tool_binds_caller_copy_and_adopt_is_not_sealed() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let launch = root.path().to_path_buf();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let spawned = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-inherit".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
    let json = outcome_json(&spawned);
    assert_eq!(json["workspace"]["id"], "ws-root");
    assert_eq!(
        std::fs::canonicalize(child_path(&tree, "c1")).unwrap(),
        std::fs::canonicalize(&launch).unwrap()
    );
    assert!(!launch.join(".plak/worktrees").exists());
    let adopt = adopt_tool(driver.clone())
        .adopt_with("op-adopt", &json!({ "candidate_id": "cand-c1-1" }))
        .unwrap();
    assert_eq!(adopt.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&adopt)["code"], "not_sealed");
    let loaded = reload(&store, "tree1");
    assert_eq!(loaded.node("c1").unwrap().workspace_id, "ws-root");
    assert_eq!(
        std::fs::canonicalize(&loaded.node("c1").unwrap().path).unwrap(),
        std::fs::canonicalize(&launch).unwrap()
    );
}

#[tokio::test]
async fn git_mint_tool_forks_from_the_parent_and_reloads() {
    let (root, store, tree) = git_persist_fixture();
    std::fs::write(root.path().join("DIRTY.txt"), "dirty\n").unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let spawned = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-mint".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
    let json = outcome_json(&spawned);
    assert_eq!(json["tier"], "mutate");
    assert_eq!(json["workspace"]["mode"], "fork");
    let workspace_id = json["workspace"]["id"].as_str().unwrap().to_owned();
    let child_path = child_path(&tree, "c1");
    assert!(child_path.starts_with(root.path().join(".plak/worktrees")));
    assert!(
        child_path.join("DIRTY.txt").is_file(),
        "parent dirty state is copied"
    );
    assert!(child_path.join("SEED.txt").is_file());
    let porcelain = Command::new("git")
        .args(["status", "--porcelain", "-uall"])
        .current_dir(root.path())
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&porcelain.stdout);
    assert!(
        porcelain.status.success() && !status.contains(".plak"),
        "mint copies must be gitignored, got:\n{status}"
    );
    let loaded = reload(&store, "tree1");
    let workspace = loaded.workspace(&workspace_id).unwrap();
    assert_eq!(workspace.mode, WorkspaceMode::Fork);
    assert_eq!(workspace.status, WorkspaceStatus::Active);
    assert_eq!(loaded.node("c1").unwrap().tier, Tier::Mutate);
    assert_eq!(
        loaded
            .node("c1")
            .unwrap()
            .template
            .as_ref()
            .map(TemplateName::as_str),
        Some("implement")
    );
}

#[tokio::test]
async fn discard_wire_waits_for_cleanup_and_replays_only_its_acknowledged_outcome() {
    use plakous_workspace::occupancy_vcs::{
        VcsError,
        fake::{FakeOp, FakeVcs},
    };
    for fail in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "cleanup-wire",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let store: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
        let driver = driver_on(tree, store.clone());
        let spawned = spawn_tool(driver.clone(), None)
            .spawn_with(
                "cleanup-spawn".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
        let id = "c1";
        let workspace_id = outcome_json(&spawned)["workspace"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let path = child_path(&driver.tree, id);
        vcs.seal_next_with(Ok(FakeVcs::base_for(id)));
        // Finished, not closed: a closed copy would already be pruned.
        driver
            .mutate_and_settle(|tree| {
                tree.record_turn_completed(id).unwrap();
                driver.save(tree).unwrap();
            })
            .unwrap();
        let request = WorkspaceIdRequest {
            workspace_id: workspace_id.clone(),
        };
        let (pending, effects) = driver
            .mutate(|tree| {
                let pending = tree
                    .discard_workspace("root", &request, Some("cleanup-discard"))
                    .unwrap();
                driver.save(tree).unwrap();
                pending
            })
            .unwrap();
        assert_eq!(pending.status, WorkspaceStatus::Discarding);
        let tool = discard_tool(driver.clone());
        let arguments = workspace_request(&workspace_id);
        assert_eq!(
            tool.run_named("cleanup-discard", &arguments)
                .unwrap()
                .kind(),
            ToolOutcomeKind::Failure
        );
        assert!(
            driver.tree.lock().unwrap().operations()["cleanup-discard"]
                .terminal_code
                .is_none()
        );
        assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
        assert!(path.exists());
        let paused = reload(&store, "cleanup-wire");
        assert_eq!(
            paused.workspace(&workspace_id).unwrap().status,
            WorkspaceStatus::Discarding
        );
        assert_eq!(
            paused.node(id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        if fail {
            vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected cleanup failure")));
        }
        assert_eq!(driver.settle(effects).is_err(), fail);
        let replay = tool.run_named("cleanup-discard", &arguments).unwrap();
        assert_eq!(
            replay.kind(),
            if fail {
                ToolOutcomeKind::Failure
            } else {
                ToolOutcomeKind::Success
            }
        );
        let loaded = reload(&store, "cleanup-wire");
        assert_eq!(
            loaded.workspace(&workspace_id).unwrap().status,
            if fail {
                WorkspaceStatus::Discarding
            } else {
                WorkspaceStatus::Discarded
            }
        );
        assert_eq!(path.exists(), fail);
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
        if fail {
            assert_eq!(outcome_json(&replay)["code"], "workspace_recovery_required");
        } else {
            assert_eq!(outcome_json(&replay)["status"], "discarded");
        }
    }
}

struct CleanupCutStore {
    inner: MemoryTreeStore,
    remaining: Mutex<Option<usize>>,
}

impl TreeStore for CleanupCutStore {
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
        let mut remaining = self.remaining.lock().unwrap();
        if let Some(count) = remaining.as_mut() {
            if *count == 0 {
                return Err(crate::tree::TreeStoreError::unavailable(
                    "cleanup publication cut",
                ));
            }
            *count -= 1;
        }
        self.inner.save(tree)
    }
}

#[tokio::test]
async fn discard_publication_cuts_never_remove_unprepared_or_replay_unpublished_success() {
    use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
    for cut in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "cleanup-cut",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let store = Arc::new(CleanupCutStore {
            inner: MemoryTreeStore::new(),
            remaining: Mutex::new(None),
        });
        let driver = driver_on(tree, store.clone());
        let spawned = spawn_tool(driver.clone(), None)
            .spawn_with(
                "spawn-cut".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        let workspace = outcome_json(&spawned)["workspace"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let path = child_path(&driver.tree, "c1");
        vcs.seal_next_with(Ok(FakeVcs::base_for("c1")));
        // Finished, not closed: a closed copy would already be pruned.
        driver
            .mutate_and_settle(|tree| {
                tree.record_turn_completed("c1").unwrap();
                driver.save(tree).unwrap();
            })
            .unwrap();
        *store.remaining.lock().unwrap() = Some(cut);
        let tool = discard_tool(driver.clone());
        let args = workspace_request(&workspace);
        assert_eq!(
            tool.run_named("discard-cut", &args).unwrap().kind(),
            ToolOutcomeKind::Failure
        );
        assert_eq!(
            tool.run_named("discard-cut", &args).unwrap().kind(),
            ToolOutcomeKind::Failure
        );
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), usize::from(cut == 2));
        assert_eq!(path.exists(), cut == 1);
        let durable = store.inner.load("cleanup-cut").unwrap().unwrap();
        let reopened = ProductAgentTree::reopen(durable);
        assert!(
            reopened.operations()["discard-cut"]
                .terminal_response
                .is_none()
        );
        *store.remaining.lock().unwrap() = None;
        let replay = tool.run_named("discard-cut", &args).unwrap();
        assert_eq!(
            replay.kind(),
            if cut == 2 {
                ToolOutcomeKind::Success
            } else {
                ToolOutcomeKind::Failure
            }
        );
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), usize::from(cut == 2));
    }
}

#[tokio::test]
async fn closing_a_child_prunes_its_copy() {
    let (_root, store, tree) = git_persist_fixture();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let spawned = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-mint".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    let json = outcome_json(&spawned);
    let child_path = child_path(&tree, "c1");
    let workspace_id = json["workspace"]["id"].as_str().unwrap().to_owned();
    let closed = close_tool(driver.clone(), None)
        .close_with("op-close".into(), &agent_id_request("c1"))
        .await
        .unwrap();
    assert_eq!(closed.kind(), ToolOutcomeKind::Success);
    // The copy never diverged, so no candidate holds it: closing removes
    // the worktree and its branch without a discard.
    assert!(!child_path.exists());
    let loaded = reload(&store, "tree1");
    assert_eq!(
        loaded.workspace(&workspace_id).unwrap().status,
        WorkspaceStatus::Discarded
    );
}

#[tokio::test]
async fn minted_child_policy_cannot_escape_to_parent_seed() {
    let (root, store, tree) = git_persist_fixture();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let spawned = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-mint".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
    let child_path = child_path(&tree, "c1");
    let policy = WorkspacePolicy::host(&child_path, Path::new(".")).unwrap();
    assert!(policy.resolve_existing("SEED.txt").is_ok());
    assert!(policy.resolve_existing("../../SEED.txt").is_err());
    assert!(root.path().join("SEED.txt").is_file());
    assert!(
        child_path.join(".plak/bin/git").is_file(),
        "minted copy must install the git-redirect shim"
    );
}

#[tokio::test]
async fn jj_mint_tool_mints_under_plak_workspaces_when_jj_exists() {
    if !Command::new("jj")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".jj")).unwrap();
        std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let first = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-jj-mint".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&first)["code"], "mint_unavailable");
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded.operations().get("op-jj-mint").unwrap().terminal_code,
            Some(TopologyErrorCode::MintUnavailable)
        );
        return;
    }
    let (repo, store, tree) = jj_persist_fixture();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let first = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-jj-mint".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.kind(), ToolOutcomeKind::Success);
    let child_path = child_path(&tree, "c1");
    assert!(
        child_path
            .display()
            .to_string()
            .contains(".plak/workspaces/"),
        "jj mint must land under .plak/workspaces/"
    );
    assert!(child_path.join(".plak/bin/jj").is_file());
    let replay = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-jj-mint".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay.kind(), ToolOutcomeKind::Success);
    let loaded = reload(&store, "tree1");
    assert_eq!(loaded.nodes().len(), 2);
    assert!(repo.path().join("SEED.txt").is_file());
}

#[tokio::test]
async fn no_vcs_mint_tool_is_unavailable() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let first = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-novcs-mint".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&first)["code"], "mint_unavailable");
    let loaded = reload(&store, "tree1");
    assert_eq!(
        loaded
            .operations()
            .get("op-novcs-mint")
            .unwrap()
            .terminal_code,
        Some(TopologyErrorCode::MintUnavailable)
    );
}

#[tokio::test]
async fn discard_tool_refuses_running_occupant() {
    let (_root, store, tree) = git_persist_fixture();
    let driver = driver_on(tree, Arc::clone(&store));
    let spawned = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-mint".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    let workspace_id = outcome_json(&spawned)["workspace"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let refused = discard_tool(driver.clone())
        .run_named("op-discard-running", &workspace_request(&workspace_id))
        .unwrap();
    assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&refused)["code"], "discard_not_applicable");
    let loaded = reload(&store, "tree1");
    assert_eq!(
        loaded.workspace(&workspace_id).unwrap().status,
        WorkspaceStatus::Active
    );
}

/// At the tools layer: `close_agent` on an idle copy seals
/// through the deferred seam and lands the candidate after the lock drops.
#[tokio::test]
async fn close_tool_seals_an_idle_copy_after_the_lock_drops() {
    let (_root, store, mut tree) = git_persist_fixture();
    let child = tree
        .spawn_agent("root", &mint_spawn_request(), None)
        .unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let outcome = close_tool(driver.clone(), None)
        .close_with("op-close-seal".into(), &agent_id_request(&child.agent_id))
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    {
        let mut tree = tree.lock().unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert_eq!(tree.candidates().len(), 1, "the idle closed copy sealed");
        assert!(tree.take_effects().is_empty(), "no ticket was stranded");
    }
    let loaded = reload(&store, "tree1");
    assert_eq!(loaded.candidates().len(), 1, "the seal was persisted");
}

/// the tools-layer adopt lands with the lock released and
/// records a replayable receipt; a refusal closes the operation too.
#[tokio::test]
async fn adopt_with_running_jobs_succeeds_and_warns_by_id() {
    let (root, store, mut tree) = git_persist_fixture();
    let child = tree
        .spawn_agent("root", &mint_spawn_request(), None)
        .unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = tree
        .attempt(&child.agent_id)
        .unwrap()
        .candidate_id
        .clone()
        .unwrap();
    let driver = driver_on(tree, store);
    let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
    let tool = adopt_tool(driver.clone());
    let outcome = tool
        .adopt_with("adopt-running-job", &json!({"candidate_id": candidate_id}))
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let ToolResultPart::Text(block) = &outcome.parts()[0] else {
        panic!("text receipt required")
    };
    let receipt: Value = serde_json::from_str(block.text()).unwrap();
    assert!(
        receipt["warning"]
            .as_str()
            .unwrap()
            .contains("running processes p1")
    );
    assert_eq!(receipt["candidate_id"], json!(candidate_id));
    assert_eq!(jobs.jobs()[0].status, plakous::tools::JobStatus::Running);
    jobs.stop_jobs(
        plakous::tools::JobStopReason::Model,
        std::time::Duration::from_millis(250),
    )
    .unwrap();
    let replay = tool
        .adopt_with("adopt-running-job", &json!({"candidate_id": candidate_id}))
        .unwrap();
    assert_eq!(
        outcome_json(&replay),
        receipt,
        "warning is the stored observation, not recomputed on replay"
    );
}

#[tokio::test]
async fn adopt_tool_lands_outside_the_lock_and_records_the_receipt() {
    let (root, store, mut tree) = git_persist_fixture();
    let child = tree
        .spawn_agent("root", &mint_spawn_request(), None)
        .unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = tree
        .attempt(&child.agent_id)
        .unwrap()
        .candidate_id
        .clone()
        .unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let tool = adopt_tool(driver.clone());
    let request = serde_json::to_value(AdoptCandidateRequest {
        candidate_id: candidate_id.clone(),
        expected_target: None,
        evidence: None,
    })
    .unwrap();
    let outcome = tool.adopt_with("op-adopt", &request).unwrap();
    assert_eq!(
        outcome.kind(),
        ToolOutcomeKind::Success,
        "{}",
        outcome_json(&outcome)
    );
    assert_eq!(outcome_json(&outcome)["files_changed"], 1);
    assert_eq!(
        std::fs::read_to_string(root.path().join("H.txt")).unwrap(),
        "child\n"
    );
    let replay = tool.adopt_with("op-adopt", &request).unwrap();
    assert_eq!(outcome_json(&replay), outcome_json(&outcome));
    let refused = tool
        .adopt_with(
            "op-adopt-unknown",
            &serde_json::to_value(AdoptCandidateRequest {
                candidate_id: CandidateId::from_persisted("cand-nope".to_owned()),
                expected_target: None,
                evidence: None,
            })
            .unwrap(),
        )
        .unwrap();
    assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&refused)["code"], "not_sealed");
    let tree = tree.lock().unwrap();
    assert_eq!(
        tree.operations()["op-adopt-unknown"].terminal_code,
        Some(TopologyErrorCode::NotSealed)
    );
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Adopted
    );
}
