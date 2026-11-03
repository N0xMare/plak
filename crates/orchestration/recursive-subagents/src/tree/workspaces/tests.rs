use crate::protocol::{SpawnAgentRequest, SpawnWorkspace, Tier, WorkspaceMode, WriterMode};
use crate::tree::test_support::*;
use crate::tree::vcs_fixtures::*;
use crate::tree::*;

#[test]
fn restored_pause_is_presented_without_rewriting_folded_node_statuses() {
    let (_tmp, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let statuses: BTreeMap<_, _> = tree
        .nodes
        .iter()
        .map(|(id, node)| (id.clone(), node.status))
        .collect();
    tree.restore_paused();
    assert_eq!(tree.state, TreeAdmissionState::RestoredPaused);
    for (id, status) in &statuses {
        assert_eq!(tree.node(id).unwrap().status, *status);
    }
    let view = view::project_tree_view(&tree);
    assert_eq!(
        view.nodes
            .iter()
            .find(|node| node.agent_id == child)
            .unwrap()
            .status,
        NodeStatus::Paused
    );
    tree.resume_tree().unwrap();
    for (id, status) in statuses {
        assert_eq!(tree.node(&id).unwrap().status, status);
    }
}

#[test]
fn mutate_defaults_to_inheriting_the_caller_checkout() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let request =
        SpawnAgentRequest::test_named("implement", Tier::Mutate, "implementer", "edit the parser");
    let spawned = tree
        .spawn_agent("root", &request, Some("op-inherit-mutate"))
        .unwrap();
    assert_eq!(spawned.tier, Tier::Mutate);
    assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
    assert_eq!(
        spawned.workspace.id,
        tree.node("root").unwrap().workspace_id
    );
}

#[test]
fn execute_defaults_to_inheriting_the_caller_checkout() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let request =
        SpawnAgentRequest::test_named("implement", Tier::Execute, "runner", "run the checks");
    let spawned = tree
        .spawn_agent("root", &request, Some("op-inherit-execute"))
        .unwrap();
    assert_eq!(spawned.tier, Tier::Execute);
    assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
    assert_eq!(
        spawned.workspace.id,
        tree.node("root").unwrap().workspace_id
    );
}

/// `workspace=fork` on `execute` mints a disposable snapshot copy —
/// detached, no branch, and read-only as a source. Omit workspace to inherit.
#[test]
fn execute_tier_mints_a_detached_snapshot_copy() {
    let repo = git_repo();
    std::fs::write(repo.path().join("DIRTY.txt"), "dirty\n").unwrap();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let mut request = mint_req();
    request.tier = Tier::Execute;
    request.workspace = SpawnWorkspace::Fork;
    let spawned = tree.spawn_agent("root", &request, Some("op-exec")).unwrap();
    assert_eq!(spawned.tier, Tier::Execute);
    assert_eq!(spawned.workspace.mode, WorkspaceMode::Snapshot);
    let path = tree.node(&spawned.agent_id).unwrap().path.clone();
    assert!(path.join("DIRTY.txt").is_file(), "the base is the snapshot");
    // A snapshot copy is thrown away unless it seals, so it takes no branch.
    assert_eq!(tree.node(&spawned.agent_id).unwrap().branch, None);
    let workspace = tree.workspace(&spawned.workspace.id).unwrap();
    assert_eq!(workspace.mode, WorkspaceMode::Snapshot);
    assert_eq!(workspace.base_commit.as_deref().map(str::len), Some(40));
    assert!(
        !tree
            .launch_checkout
            .join(".plak/worktrees")
            .join(&spawned.agent_id)
            .join(".git")
            .is_dir()
    );
}

/// A launch checkout with no git or jj identity cannot mint at all.
#[test]
fn mint_without_a_vcs_identity_is_typed_mint_unavailable() {
    let (_tmp, mut tree) = tree();
    let error = tree
        .spawn_agent("root", &mint_req(), Some("op-mint"))
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::MintUnavailable);
    assert_eq!(
        tree.operations().get("op-mint").unwrap().terminal_code,
        Some(TopologyErrorCode::MintUnavailable)
    );
    assert_eq!(tree.nodes().len(), 1, "no node is left behind");
    assert!(
        tree.epoch_allows_rebind(),
        "the pending counter is released"
    );
}

#[test]
fn resume_after_pause_does_not_restart_a_bound_turn() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.bind_session_locator(&spawned.agent_id, "session:c1")
        .unwrap();
    tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
    tree.set_active_turn(&spawned.agent_id, true).unwrap();
    tree.restore_paused();
    assert!(tree.node(&spawned.agent_id).unwrap().active_turn);
    assert_eq!(
        tree.inflight_join_targets(),
        vec![(
            spawned.agent_id.clone(),
            "session:c1".into(),
            tree.node(&spawned.agent_id)
                .unwrap()
                .path
                .display()
                .to_string(),
            "turn-1".into(),
        )]
    );
    assert_eq!(
        tree.attempt(&spawned.agent_id).unwrap().turn_id.as_deref(),
        Some("turn-1")
    );
    tree.resume_tree().unwrap();
    let mut deferred = mail(&spawned.agent_id, "again");
    deferred.priority = Some(MessagePriority::Deferred);
    let queued = tree.send_agent_message("root", &deferred, None).unwrap();
    assert_eq!(queued.disposition, MailDisposition::Queued);
    let mut urgent = mail(&spawned.agent_id, "now");
    urgent.priority = Some(MessagePriority::Urgent);
    let steered = tree.send_agent_message("root", &urgent, None).unwrap();
    assert_eq!(steered.disposition, MailDisposition::Steered);
}

#[test]
fn git_mint_forks_from_the_parent_state() {
    let repo = git_repo();
    std::fs::write(repo.path().join("DIRTY.txt"), "dirty\n").unwrap();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    assert_eq!(spawned.tier, Tier::Mutate);
    assert_eq!(spawned.workspace.mode, WorkspaceMode::Fork);
    let node = tree.node(&spawned.agent_id).unwrap();
    let path = node.path.clone();
    assert!(path.to_string_lossy().contains(".plak/worktrees/"));
    assert!(
        path.join(".plak/bin/git").is_file(),
        "minted copy must install the git-redirect shim"
    );
    assert!(
        path.join("DIRTY.txt").is_file(),
        "parent dirty state is copied"
    );
    assert!(path.join("SEED.txt").is_file());
    assert_ne!(path, repo.path());
    let porcelain = Command::new("git")
        .args(["status", "--porcelain", "-uall"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(porcelain.status.success());
    let status = String::from_utf8_lossy(&porcelain.stdout);
    assert!(
        !status.contains(".plak"),
        "mint copies must be gitignored, got:\n{status}"
    );
    let workspace = tree.workspace(&spawned.workspace.id).unwrap();
    assert_eq!(workspace.mode, WorkspaceMode::Fork);
    assert_eq!(workspace.status, WorkspaceStatus::Active);
    let base = workspace
        .base_commit
        .clone()
        .unwrap_or_else(|| unreachable!("the mint records its snapshot base"));
    assert_eq!(base.len(), 40, "{base}");
    assert_eq!(spawned.workspace.base.as_deref(), Some(base.as_str()));
    // The child's HEAD is the base snapshot, so the copy is clean: the
    // parent's dirty file is *in* the base, not copied on top of it.
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "-uall"])
        .current_dir(&path)
        .output()
        .unwrap();
    assert!(dirty.status.success());
    assert!(
        dirty.stdout.is_empty(),
        "the fork starts clean against its snapshot base: {}",
        String::from_utf8_lossy(&dirty.stdout)
    );
}

#[test]
fn jj_mint_forks_under_plak_workspaces() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&spawned.agent_id).unwrap().path.clone();
    assert!(path.to_string_lossy().contains(".plak/workspaces/"));
    assert!(
        path.join(".plak/bin/jj").is_file(),
        "minted jj copy must install the jj-op shim"
    );
    assert!(
        path.join(".plak/bin/git").is_file(),
        "minted jj copy must also install git-redirect so colocated git cannot escape"
    );
}

#[test]
fn read_only_binds_caller_copy_without_mint() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let root = tree.node("root").unwrap();
    assert_eq!(spawned.workspace.id, root.workspace_id);
    assert_eq!(tree.node(&spawned.agent_id).unwrap().path, root.path);
    assert!(!tree.launch_checkout.join(".plak/worktrees").exists());
    assert!(!tree.launch_checkout.join(".plak/bin/git").is_file());
}

#[test]
fn mutate_completion_bumps_its_fork_generation() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let counter = tree
        .generation_counter_for_root(
            tree.workspace(&spawned.workspace.id)
                .unwrap()
                .path
                .as_path(),
            &tree.workspace_paths(),
        )
        .expect("fork stamp");
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    tree.record_turn_completed(&spawned.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(tree.workspace(&spawned.workspace.id).unwrap().generation, 1);
    // The root's own checkout is untouched by a child's fork completing.
    assert_eq!(
        tree.workspace(&tree.node("root").unwrap().workspace_id)
            .unwrap()
            .generation,
        0
    );
}

#[test]
fn read_only_complete_does_not_bump_generation() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_completed(&spawned.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(tree.workspace(&spawned.workspace.id).unwrap().generation, 0);
}

#[test]
fn discard_requires_finished_occupants() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    assert_eq!(
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: spawned.workspace.id.clone(),
            },
            None,
        )
        .unwrap_err()
        .code,
        TopologyErrorCode::DiscardNotApplicable
    );
    tree.settle_for_test();
    tree.close_agent("root", &close_req(&spawned.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    let discarded = tree
        .discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: spawned.workspace.id,
            },
            None,
        )
        .unwrap();
    assert_eq!(discarded.status, WorkspaceStatus::Discarding);
}

/// Explicit discard relinquishes unadopted content as well as its copy.
#[test]
fn explicit_discard_gives_up_unadopted_candidates() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(
        tree.node(&spawned.agent_id)
            .unwrap()
            .path
            .join("retained.txt"),
        "candidate work",
    )
    .unwrap();
    tree.close_agent("root", &close_req(&spawned.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    let pinning = candidate_of(&tree, &spawned.agent_id);
    let request = WorkspaceIdRequest {
        workspace_id: spawned.workspace.id.clone(),
    };
    let output = tree
        .discard_workspace("root", &request, Some("discard-candidates"))
        .unwrap();
    assert_eq!(output.status, WorkspaceStatus::Discarding);
    assert_eq!(
        serde_json::to_value(&output).unwrap()["discarded_candidates"],
        serde_json::json!([pinning])
    );
    tree.settle_for_test();
    let replay = tree
        .discard_workspace("root", &request, Some("discard-candidates"))
        .unwrap();
    assert_eq!(replay.status, WorkspaceStatus::Discarded);
    assert_eq!(replay.discarded_candidates, output.discarded_candidates);
    assert_eq!(
        tree.workspace(&spawned.workspace.id).unwrap().status,
        WorkspaceStatus::Discarded
    );
    assert_eq!(
        tree.adopt_candidate("root", &adopt_req(&pinning), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::AdoptNotApplicable
    );
}

/// A `Sealing` node or an `Adopting` row reopens `RecoveryRequired`
/// and never auto-retries.
#[test]
fn sealing_and_adopting_reopen_recovery_required() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let sealing = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let adopting = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.record_turn_completed(&sealing.agent_id).unwrap();
    let _stranded = tree.take_effects();
    tree.workspaces
        .get_mut(&adopting.workspace.id)
        .unwrap()
        .status = WorkspaceStatus::Adopting;
    tree.restore_paused();
    assert_eq!(
        tree.node(&sealing.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_eq!(
        tree.node(&adopting.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_eq!(
        tree.node("root").unwrap().status,
        NodeStatus::Running,
        "the adopt's owner is the root, which nothing could recover"
    );
    assert!(
        tree.fault_reason("root")
            .is_some_and(|reason| reason.contains("adopt into this checkout"))
    );
    tree.resume_tree().unwrap();
    assert_eq!(
        tree.node(&sealing.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

/// An adopt lands in its owner's checkout however its producer ended, so
/// a reopen faults a non-root owner even when every occupant of the
/// adopting copy is closed; the closed producer itself stays closed.
#[test]
fn an_interrupted_adopt_faults_its_owner_even_after_the_producer_closed() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let hub = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let producer = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.record_turn_completed(&hub.agent_id).unwrap();
    tree.record_turn_completed(&producer.agent_id).unwrap();
    tree.settle_for_test();
    tree.close_agent("root", &close_req(&producer.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    let row = tree.workspaces.get_mut(&producer.workspace.id).unwrap();
    row.status = WorkspaceStatus::Adopting;
    row.owner_agent_id.clone_from(&hub.agent_id);
    tree.restore_paused();
    assert_eq!(
        tree.node(&hub.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_eq!(
        tree.node(&producer.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert_eq!(tree.node("root").unwrap().status, NodeStatus::Running);

    // Closing the owner after inspecting its checkout settles the adopt:
    // later reopens do not fault it again.
    tree.close_agent("root", &close_req(&hub.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(tree.node(&hub.agent_id).unwrap().status, NodeStatus::Closed);
    tree.workspaces
        .get_mut(&producer.workspace.id)
        .unwrap()
        .status = WorkspaceStatus::Adopting;
    tree.restore_paused();
    tree.resume_tree().unwrap();
    assert_eq!(tree.node(&hub.agent_id).unwrap().status, NodeStatus::Closed);
}

/// A copy minted from a candidate keeps that candidate's workspace
/// alive while its own occupant is unfinished.
#[test]
fn discard_refuses_while_a_snapshot_reviewer_is_bound() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let implementer = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let implementer_path = tree.node(&implementer.agent_id).unwrap().path.clone();
    std::fs::write(implementer_path.join("H.txt"), "child\n").unwrap();
    tree.record_turn_completed(&implementer.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &implementer.agent_id);
    let commit = tree.candidates().get(&candidate_id).unwrap().commit.clone();
    tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();

    // A reviewer's snapshot copy is based on that candidate.
    let mut request = mint_req();
    request.tier = Tier::Execute;
    request.workspace = SpawnWorkspace::Fork;
    let reviewer = tree.spawn_agent("root", &request, None).unwrap();
    tree.set_workspace_base_for_test(&reviewer.workspace.id, &commit, "tree", None);
    let discard = WorkspaceIdRequest {
        workspace_id: implementer.workspace.id.clone(),
    };
    let error = tree.discard_workspace("root", &discard, None).unwrap_err();
    tree.settle_for_test();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceReferenced);
    assert!(error.message.contains(&reviewer.workspace.id), "{error:?}");
    tree.close_agent("root", &close_req(&reviewer.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.discard_workspace("root", &discard, None)
            .unwrap()
            .status,
        WorkspaceStatus::Discarding
    );
    tree.settle_for_test();
}

/// A parent rewrite must never run destructive stale recovery over
/// uncommitted child files. Refuse and retain both tracked and new work.
#[test]
fn jj_uncommitted_work_is_preserved_by_typed_stale_recovery_refusal() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let first_path = tree.node(&first.agent_id).unwrap().path.clone();
    std::fs::write(first_path.join("FIRST.txt"), "first\n").unwrap();
    tree.record_turn_completed(&first.agent_id).unwrap();
    tree.settle_for_test();
    let input = candidate_of(&tree, &first.agent_id);

    let mut over = mint_req();
    over.inputs = Some(vec![input.to_string()]);
    let second = tree.spawn_agent("root", &over, None).unwrap();
    let second_path = tree.node(&second.agent_id).unwrap().path.clone();
    // Something rewrites the copy's base out from under it — an adopt of
    // its input is now refused, but a human `jj abandon` in the launch
    // checkout is not — and the child then writes into a copy jj considers
    // stale.
    let change = tree
        .candidates()
        .get(&input)
        .unwrap()
        .change_id
        .clone()
        .unwrap();
    jj_in(repo.path(), &["abandon", change.as_str()]);
    std::fs::write(second_path.join("SECOND.txt"), "second\n").unwrap();
    tree.record_turn_completed(&second.agent_id).unwrap();
    tree.settle_for_test();

    assert_eq!(
        std::fs::read_to_string(second_path.join("SECOND.txt")).unwrap(),
        "second\n"
    );
    let attempt = tree.attempt(&second.agent_id).unwrap();
    assert!(attempt.candidate_id.is_none());
    let failure = attempt.failure.as_ref().unwrap();
    assert_eq!(failure.kind, ProductAgentTree::SEAL_FAILED_KIND);
    assert!(
        failure.message.contains("recovery required"),
        "{}",
        failure.message
    );
    assert_eq!(
        tree.node(&second.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

/// B9: an idle finished child whose copy was discarded cannot be
/// re-tasked into a path that no longer exists.
#[test]
fn followup_is_refused_into_a_discarded_workspace() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    // No writes: nothing seals, so the copy is discardable.
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    tree.discard_workspace(
        "root",
        &WorkspaceIdRequest {
            workspace_id: child.workspace.id.clone(),
        },
        None,
    )
    .unwrap();
    tree.settle_for_test();
    let mut followup = mail(&child.agent_id, "one more thing");
    followup.purpose = Some(MessagePurpose::Followup);
    let error = tree
        .send_agent_message("root", &followup, None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::FollowupWorkspaceGone);
    assert!(error.message.contains("discarded"), "{}", error.message);
    assert_eq!(
        tree.attempt(&child.agent_id).unwrap().attempt_id,
        child.attempt_id,
        "no second attempt was opened"
    );
}

/// The tree drives minting and unminting through its `VcsOps` port and
/// nothing else: on `FakeVcs` a mutate spawn on a plain temp dir mints a
/// recorded copy with the fake's base, and a discard unmints it.
#[test]
fn fake_vcs_mints_and_unmints_without_a_repo() {
    let (root, vcs, mut tree) = fake_tree();
    assert_eq!(
        tree.node("root").unwrap().branch.as_deref(),
        Some("fake-main"),
        "the root label came from the port"
    );
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let node = tree.node(&child.agent_id).unwrap().clone();
    assert!(
        node.path
            .starts_with(root.path().join(".plak").join("fake"))
    );
    assert!(node.path.is_dir());
    assert_eq!(
        node.branch.as_deref(),
        Some(
            plakous_workspace::occupancy_vcs::copy_names(
                &digest_text(&tree.tree_id)[..8],
                &child.agent_id
            )
            .branch
            .as_str()
        )
    );
    let row = tree.workspace(&node.workspace_id).unwrap();
    assert_eq!(
        row.base_commit.as_deref(),
        Some(FakeVcs::base_for(&child.agent_id).commit.as_str())
    );
    assert_eq!(vcs.calls_of(FakeOp::Mint).len(), 1);
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    tree.adopt_candidate(
        "root",
        &adopt_req(&candidate_of(&tree, &child.agent_id)),
        None,
    )
    .unwrap();
    tree.discard_workspace(
        "root",
        &WorkspaceIdRequest {
            workspace_id: node.workspace_id.clone(),
        },
        None,
    )
    .unwrap();
    tree.settle_for_test();
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
    assert!(!node.path.exists(), "the fake unmint removed the copy");
}

#[test]
fn failed_removal_keeps_copy_and_prepared_discard_cannot_replay_success() {
    let (_root, vcs, mut tree) = fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    vcs.seal_next_with(Ok(FakeVcs::base_for(&child.agent_id)));
    // Finished, not closed: a closed copy would already be pruned.
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let request = WorkspaceIdRequest {
        workspace_id: child.workspace.id.clone(),
    };
    let pending = tree
        .discard_workspace("root", &request, Some("discard-fails"))
        .unwrap();
    assert_eq!(pending.status, WorkspaceStatus::Discarding);
    assert!(
        tree.operations()["discard-fails"]
            .terminal_response
            .is_none()
    );
    assert!(path.exists());
    vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected removal failure")));
    let settled = tree.settle_for_test();
    assert_eq!(settled.cleanup_failures.len(), 1);
    assert!(path.exists());
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Discarding
    );
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_eq!(
        tree.operations()["discard-fails"].terminal_code,
        Some(TopologyErrorCode::WorkspaceRecoveryRequired)
    );
    assert_eq!(
        tree.discard_workspace("root", &request, Some("discard-fails"))
            .unwrap_err()
            .code,
        TopologyErrorCode::WorkspaceRecoveryRequired
    );
    assert!(tree.take_effects().is_empty());
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
}

/// A finished copy with no candidate whose explicit discard fails: the row
/// stays `discarding` and its occupant `recovery_required`.
fn failed_discard_of_a_finished_copy(
    tree: &mut ProductAgentTree,
    vcs: &FakeVcs,
) -> (String, String, PathBuf) {
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    vcs.seal_next_with(Ok(FakeVcs::base_for(&child.agent_id)));
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    tree.discard_workspace(
        "root",
        &WorkspaceIdRequest {
            workspace_id: child.workspace.id.clone(),
        },
        None,
    )
    .unwrap();
    vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected removal failure")));
    assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    (child.agent_id, child.workspace.id, path)
}

/// Closing the occupants of a failed removal is the operator's settlement
/// of it: the prune retries the removal once, and the copy ends
/// `discarded`. A retry that fails again keeps the row reserved, faults
/// nobody, and is not repeated in this process.
#[test]
fn closing_a_failed_removal_retries_it_as_a_prune_once() {
    let (_root, vcs, mut tree) = fake_tree();
    let (child, workspace, path) = failed_discard_of_a_finished_copy(&mut tree, &vcs);
    tree.close_agent("root", &close_req(&child), None).unwrap();
    tree.settle_for_test();
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
    assert_eq!(
        tree.workspace(&workspace).unwrap().status,
        WorkspaceStatus::Discarded
    );
    assert!(!path.exists());
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 2);

    let (child, workspace, path) = failed_discard_of_a_finished_copy(&mut tree, &vcs);
    tree.close_agent("root", &close_req(&child), None).unwrap();
    vcs.unmint_next_with(Err(VcsError::mint_unavailable("still refused")));
    tree.settle_for_test();
    assert_eq!(
        tree.workspace(&workspace).unwrap().status,
        WorkspaceStatus::Discarding
    );
    assert!(path.exists());
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
    assert_eq!(tree.node("root").unwrap().status, NodeStatus::Running);
    let removals = vcs.calls_of(FakeOp::Unmint).len();
    tree.close_agent("root", &close_req(&child), None).unwrap();
    tree.settle_for_test();
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), removals);
}

/// An explicit discard gives up the copy's unadopted candidates, so a
/// failed one is still retried once its occupant closes; the candidate
/// stays unadoptable.
#[test]
fn a_failed_discard_of_a_copy_with_a_candidate_is_retried_on_close() {
    let (_root, vcs, mut tree) = fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate = candidate_of(&tree, &child.agent_id);
    tree.discard_workspace(
        "root",
        &WorkspaceIdRequest {
            workspace_id: child.workspace.id.clone(),
        },
        None,
    )
    .unwrap();
    vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected removal failure")));
    assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
    tree.close_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Discarded
    );
    assert_eq!(
        tree.adopt_candidate("root", &adopt_req(&candidate), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::AdoptNotApplicable
    );
}

/// The reopen law for a stale `discarding` row: its open occupant is
/// faulted and the owner is not; once the occupant is closed nobody is,
/// and a root an earlier build faulted for it reopens running, so its
/// waits are not refused. Nothing is retried until a prune runs.
#[test]
fn a_stale_removal_never_faults_the_root_or_a_closed_occupant_on_reopen() {
    let (_root, vcs, mut tree) = fake_tree();
    let (child, workspace, _path) = failed_discard_of_a_finished_copy(&mut tree, &vcs);
    tree.restore_paused();
    assert_eq!(
        tree.node(&child).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_eq!(tree.node("root").unwrap().status, NodeStatus::Running);

    tree.close_agent("root", &close_req(&child), None).unwrap();
    let _not_run = tree.take_effects();
    tree.nodes.get_mut("root").unwrap().status = NodeStatus::RecoveryRequired;
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert_eq!(reopened.node("root").unwrap().status, NodeStatus::Running);
    assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Closed);
    assert_eq!(
        reopened.workspace(&workspace).unwrap().status,
        WorkspaceStatus::Discarding
    );
    assert!(reopened.take_effects().is_empty());
    reopened.resume_tree().unwrap();
    assert_eq!(reopened.node("root").unwrap().status, NodeStatus::Running);
    assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Closed);
    assert!(reopened.poll_wait("root", &wait_for(&child)).is_ok());

    reopened
        .close_agent("root", &close_req(&child), None)
        .unwrap();
    reopened.prune_closed_copies();
    assert_eq!(reopened.take_effects().unmints.len(), 1);
}

#[test]
fn failed_hook_cleanup_retains_recovery_copy() {
    let (_root, vcs, mut tree) = fake_tree();
    let request = mint_req();
    let child = tree.spawn_agent("root", &request, None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    tree.fail_mint_hook(
        "root",
        &request,
        Some("hook-fails"),
        &child.agent_id,
        "injected hook failure",
    );
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Discarding
    );
    vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected cleanup failure")));
    assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
    assert!(path.exists());
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(
        tree.fault_reason(&child.agent_id)
            .unwrap()
            .contains("removal failed")
    );
}

/// On the port: the hook the tree hands out runs through
/// `VcsOps::run_mint_hook`, and its `HookFailed` detail is what
/// `fail_mint_hook` records while the fake unmints the copy.
#[test]
fn fake_vcs_mint_hook_failure_unmints_through_the_port() {
    let (_root, vcs, mut tree) = fake_tree();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    binding.on_mint = Some("true".into());
    tree.pin_template("implement", binding);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    let caller_path = tree.node("root").unwrap().path.clone();
    let hook = tree
        .mint_hook(
            &child.agent_id,
            "root",
            &caller_path,
            &mint_req(),
            Some("op-1"),
        )
        .unwrap();
    vcs.hook_next_with(Err(VcsError::hook_failed("exited 3")));
    let failed = block_on_for_test(VcsOps::run_mint_hook(
        vcs.as_ref(),
        &hook.cwd,
        &hook.command,
        &hook.env,
    ))
    .unwrap_err();
    assert_eq!(failed.kind, VcsErrorKind::HookFailed);
    assert_eq!(vcs.calls_of(FakeOp::Hook).len(), 1);
    let error = tree.fail_mint_hook(
        "root",
        &mint_req(),
        Some("op-1"),
        &child.agent_id,
        &failed.detail,
    );
    tree.settle_for_test();
    assert_eq!(error.code, TopologyErrorCode::MintHookFailed);
    assert!(error.message.contains("exited 3"));
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
    assert!(!child_path.exists(), "the fake unmint removed the copy");
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Failed
    );
}

#[test]
fn mint_hook_keeps_the_planned_parent_path_if_the_caller_row_is_missing() {
    let (_root, vcs, mut tree) = fake_tree();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    binding.on_mint = Some("true".into());
    tree.pin_template("implement", binding);
    let parent_path = tree.node("root").unwrap().path.clone();
    let SpawnAdmission::Reserved(reservation, Some(ticket)) = tree
        .spawn_reserve("root", &mint_req(), None, &Ok(BTreeSet::new()))
        .unwrap()
    else {
        panic!("a fork spawn must reserve a mint");
    };
    let minted = block_on_for_test(ticket.mint(vcs.as_ref())).unwrap();
    tree.nodes.remove("root");
    let child = tree.spawn_commit(*reservation, Some(minted)).unwrap();
    let hook = tree
        .effects
        .hooks
        .pop()
        .expect("on_mint must not be skipped");
    assert_eq!(hook.agent_id, child.agent_id);
    assert_eq!(
        hook.env.get("PLAK_PARENT_PATH"),
        Some(&parent_path.display().to_string())
    );
}

#[test]
fn mint_hook_receives_authoritative_copy_context_for_nested_spawns() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    tree.grants.get_mut("root").unwrap().max_depth = 2;
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    binding.spawns = vec![TemplateName::parse("implement").unwrap()];
    binding.on_mint = Some(
        "printf '%s\\n' \"$PLAK_LAUNCH_CHECKOUT\" \"$PLAK_PARENT_PATH\" \"$PLAK_COPY_ID\" \"$PLAK_TEST_MARK\" > context.txt".into(),
    );
    for name in ["PLAK_LAUNCH_CHECKOUT", "PLAK_PARENT_PATH", "PLAK_COPY_ID"] {
        binding.env.insert(name.into(), "template-override".into());
    }
    binding.env.insert("PLAK_TEST_MARK".into(), "minted".into());
    tree.pin_template("supervisor", binding.clone());
    tree.pin_template("implement", binding);

    let mut caller_id = "root".to_owned();
    for template in ["supervisor", "implement"] {
        let mut request = mint_req();
        request.template = TemplateName::parse(template).unwrap();
        let parent_path = tree.node(&caller_id).unwrap().path.clone();
        let child = tree.spawn_agent(&caller_id, &request, None).unwrap();
        let hook = tree
            .mint_hook(&child.agent_id, &caller_id, &parent_path, &request, None)
            .unwrap();
        let expected = [
            tree.launch_checkout.display().to_string(),
            parent_path.display().to_string(),
            copy_id(&tree.tree_id, &child.agent_id),
            "minted".into(),
        ];
        for (name, value) in ["PLAK_LAUNCH_CHECKOUT", "PLAK_PARENT_PATH", "PLAK_COPY_ID"]
            .into_iter()
            .zip(&expected)
        {
            assert_eq!(hook.env.get(name), Some(value), "{name}");
        }
        plakous_workspace::occupancy_vcs::run_mint_hook(&hook.cwd, &hook.command, &hook.env)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(hook.cwd.join("context.txt")).unwrap(),
            format!("{}\n", expected.join("\n"))
        );
        caller_id = child.agent_id;
    }
}

/// the template's `on_mint` is read for a fresh copy, runs
/// in it with the template env, and a failure unmints and fails closed.
#[test]
fn mint_hook_runs_in_the_copy_and_failure_unmints() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    binding.on_mint = Some("printf %s \"$PLAK_TEST_MARK\" > MARK.txt".into());
    binding.env.insert("PLAK_TEST_MARK".into(), "minted".into());
    tree.pin_template("implement", binding);
    assert!(
        tree.mint_hook("root", "root", repo.path(), &mint_req(), None)
            .is_none(),
        "the root shares no minted copy"
    );
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    let hook = tree
        .mint_hook(
            &child.agent_id,
            "root",
            repo.path(),
            &mint_req(),
            Some("op-1"),
        )
        .expect("the template declares a hook");
    assert_eq!(hook.cwd, child_path);
    assert_eq!(
        hook.env.get("PLAK_TEST_MARK").map(String::as_str),
        Some("minted")
    );
    plakous_workspace::occupancy_vcs::run_mint_hook(&hook.cwd, &hook.command, &hook.env).unwrap();
    assert_eq!(
        std::fs::read_to_string(child_path.join("MARK.txt")).unwrap(),
        "minted"
    );
    // A read_only child has no copy and therefore no hook.
    let reader = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert!(
        tree.mint_hook(&reader.agent_id, "root", repo.path(), &spawn_req(), None)
            .is_none()
    );

    // A failing hook: the copy is gone and the spawn is a typed failure.
    let failing = plakous_workspace::occupancy_vcs::run_mint_hook(
        &child_path,
        "echo no >&2; exit 3",
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(failing.contains("exited 3"), "{failing}");
    assert!(failing.contains("no"), "{failing}");
    let error = tree.fail_mint_hook("root", &mint_req(), Some("op-1"), &child.agent_id, &failing);
    tree.settle_for_test();
    assert_eq!(error.code, TopologyErrorCode::MintHookFailed);
    assert!(!child_path.exists(), "the copy was unminted");
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Failed
    );
    let attempt = tree.attempt(&child.agent_id).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Failed);
    assert_eq!(
        attempt
            .failure
            .as_ref()
            .map(|failure| failure.kind.as_str()),
        Some("mint_hook_failed")
    );
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Discarded
    );
    assert_eq!(
        tree.operations()["op-1"].terminal_code,
        Some(TopologyErrorCode::MintHookFailed),
        "a retry replays the refusal, not a receipt for a copy that is gone"
    );
}
