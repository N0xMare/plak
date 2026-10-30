use super::NodeOccupancy;
use crate::tree::test_support::*;
use crate::tree::vcs_fixtures::*;
use crate::tree::*;

#[test]
fn failed_candidate_rows_name_the_partial_candidate_and_fresh_spawn_recipe() {
    for kind in ["turn_failed", "context_exhausted"] {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let candidate = CandidateId::from_persisted(format!("cand-{}", child.attempt_id));
        let attempt = tree.attempts.get_mut(&child.attempt_id).unwrap();
        attempt.candidate_id = Some(candidate.clone());
        attempt.failure = Some(AttemptFailure {
            kind: kind.into(),
            message: "original cause".into(),
        });
        let expected = format!(
            "partial candidate {candidate}; spawn(inputs=[\"{candidate}\"]) with a handoff of completed work and the remaining task"
        );
        let wait = tree.wait_row(&child.agent_id).unwrap();
        assert!(wait.failure.unwrap().message.contains(&expected));
        let list = tree
            .full_list_json(
                tree.node(&child.agent_id).unwrap(),
                NodeOccupancy::default(),
            )
            .unwrap();
        assert!(
            list["failure"]["message"]
                .as_str()
                .unwrap()
                .contains(&expected)
        );
        assert_eq!(
            tree.attempt(&child.agent_id)
                .unwrap()
                .failure
                .as_ref()
                .unwrap()
                .message,
            "original cause"
        );
        tree.attempts.get_mut(&child.attempt_id).unwrap().status = AttemptStatus::Failed;
        tree.nodes.get_mut(&child.agent_id).unwrap().status = NodeStatus::Failed;
        tree.enqueue_result_mail(&child.agent_id);
        let notice: serde_json::Value =
            serde_json::from_str(&tree.pending_result_mail("root")[0].body).unwrap();
        assert_eq!(notice["continuation"], expected);
    }
}

/// A failed child must say why. `recovery_required` with a null output and
/// no message left the operator nothing to act on.
#[test]
fn a_failed_attempt_carries_a_typed_reason_on_the_wait_row() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_fault(
        &spawned.agent_id,
        "child turn failed (Model): 429 from codex",
    );
    tree.record_host_materialization(&spawned.agent_id)
        .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();

    let failure = tree
        .attempt(&spawned.agent_id)
        .unwrap()
        .failure
        .clone()
        .expect("the attempt carries why it failed");
    assert_eq!(failure.kind, "session_materialization");
    assert!(failure.message.contains("429"), "{}", failure.message);

    let settled = tree
        .poll_wait("root", &wait_for(&spawned.agent_id))
        .unwrap()
        .unwrap();
    assert_eq!(settled.agents[0].failure.as_ref(), Some(&failure));
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: None,
                include_completed: Some(true),
            },
        )
        .unwrap();
    assert_eq!(
        listed.agents[0]["failure"]["kind"],
        "session_materialization"
    );
}

/// A list row previews a long prompt and result; the wait row keeps the
/// whole result.
#[test]
fn list_rows_preview_long_prompts_and_results() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let long = format!("{{\"text\":\"{}\"}}", "y".repeat(3000));
    for attempt in tree.attempts.values_mut() {
        attempt.output_json = Some(long.clone());
    }
    tree.nodes.get_mut(&spawned.agent_id).unwrap().prompt = "p".repeat(2000);
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: None,
                include_completed: Some(true),
            },
        )
        .unwrap();
    let row = &listed.agents[0];
    assert!(row.get("output_json").is_none());
    assert_eq!(row["output_bytes"], long.len());
    assert_eq!(row["output_preview"].as_str().unwrap().len(), 1024);
    assert!(row.get("prompt").is_none());
    assert_eq!(row["prompt_bytes"], 2000);
    let waited = tree.wait_row(&spawned.agent_id).unwrap();
    assert_eq!(waited.output_json.as_deref(), Some(long.as_str()));
}

#[test]
fn empty_working_set_does_not_advertise_spawn_agent() {
    let (_tmp, mut tree) = tree();
    assert!(
        tree.compiled_tool_names("root")
            .unwrap()
            .contains(&"spawn_agent")
    );
    tree.snapshot.roster.clear();
    tree.snapshot.digest = tree.snapshot.compute_digest();
    let names = tree.compiled_tool_names("root").unwrap();
    assert!(!names.contains(&"spawn_agent"));
    assert!(names.contains(&"list_agents"));
}

#[test]
fn list_agents_reports_dirty_worktree_and_template() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&spawned.agent_id).unwrap().path.clone();
    std::fs::write(path.join("DIRTY_LIST.txt"), "dirty\n").unwrap();
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(false),
            },
        )
        .unwrap();
    let child = listed
        .agents
        .iter()
        .find(|row| row["agent_id"] == spawned.agent_id)
        .unwrap();
    assert_eq!(child["dirty"], true);
    assert_eq!(child["template"], "implement");
    assert_eq!(child["tier"], "mutate");
    assert_eq!(child["workspace_mode"], "fork");
    assert_eq!(child["attempt"], "running");
    assert_eq!(child["attempt_id"], spawned.attempt_id.as_str());
    assert!(child.get("candidate").is_none());
    assert!(child.get("lease_holder").is_none());
}

#[test]
fn list_agents_reports_ahead_behind_on_git_worktree() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&spawned.agent_id).unwrap().path.clone();
    std::fs::write(path.join("AHEAD.txt"), "ahead\n").unwrap();
    git_commit(&path, &["AHEAD.txt"], "child ahead");
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(false),
            },
        )
        .unwrap();
    let child = listed
        .agents
        .iter()
        .find(|row| row["agent_id"] == spawned.agent_id)
        .unwrap();
    assert!(child.get("ahead").is_some());
    assert!(child.get("behind").is_some());
    assert!(child["ahead"].as_u64().unwrap_or(0) >= 1);
}

#[test]
fn list_row_names_candidate_commit_and_base() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(
        tree.node(&spawned.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.record_turn_completed(&spawned.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &spawned.agent_id);
    let record = tree.candidates().get(&candidate_id).unwrap();
    let commit = record.commit.clone();
    let base = record.base_commit.clone();
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(true),
            },
        )
        .unwrap();
    let child = listed
        .agents
        .iter()
        .find(|row| row["agent_id"] == spawned.agent_id)
        .unwrap();
    assert_eq!(child["candidate"], candidate_id.as_str());
    assert_eq!(child["candidate_commit"], commit);
    assert_eq!(child["candidate_base"], base);
    let record = tree.candidates.get_mut(&candidate_id).unwrap();
    record.commit.clear();
    record.base_commit.clear();
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(true),
            },
        )
        .unwrap();
    let child = listed
        .agents
        .iter()
        .find(|row| row["agent_id"] == spawned.agent_id)
        .unwrap();
    assert!(child.get("candidate_commit").is_none());
    assert!(child.get("candidate_base").is_none());
}

#[test]
fn list_agents_reports_ahead_behind_on_jj_workspace() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&spawned.agent_id).unwrap().path.clone();
    let fresh = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(false),
            },
        )
        .unwrap();
    let row = fresh
        .agents
        .iter()
        .find(|row| row["agent_id"] == spawned.agent_id)
        .unwrap();
    assert_eq!(row["ahead"], 0);
    assert_eq!(row["behind"], 0);
    std::fs::write(path.join("AHEAD.txt"), "ahead\n").unwrap();
    jj_commit(&path, &["AHEAD.txt"], "child ahead");
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(false),
            },
        )
        .unwrap();
    let child = listed
        .agents
        .iter()
        .find(|row| row["agent_id"] == spawned.agent_id)
        .unwrap();
    assert!(child.get("ahead").is_some());
    assert!(child.get("behind").is_some());
    assert!(child.get("workspace_generation").is_none());
    assert!(child["ahead"].as_u64().unwrap_or(0) >= 1);
}

#[test]
fn jj_request_key_replays_the_admitted_input_without_reminting() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let producer = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(
        tree.node(&producer.agent_id)
            .unwrap()
            .path
            .join("result.txt"),
        "result\n",
    )
    .unwrap();
    tree.record_turn_completed(&producer.agent_id).unwrap();
    tree.settle_for_test();
    let candidate = candidate_of(&tree, &producer.agent_id);
    let mut request = mint_req();
    request.request_key = Some("jj-input".into());
    request.inputs = Some(vec![candidate.to_string()]);
    let first = tree.spawn_agent("root", &request, None).unwrap();
    let before = tree.nodes().len();
    assert_eq!(
        tree.workspace(&first.workspace.id)
            .unwrap()
            .base_commit
            .as_deref(),
        Some(tree.candidates.get(&candidate).unwrap().commit.as_str())
    );
    assert_eq!(
        tree.spawn_agent("root", &request, None).unwrap().agent_id,
        first.agent_id
    );
    request.inputs = None;
    assert_eq!(
        tree.spawn_agent("root", &request, None).unwrap_err().code,
        TopologyErrorCode::RequestKeyConflict
    );
    assert_eq!(tree.nodes().len(), before);
}

/// B5: `request_key` identity covers `inputs`, and a key spent by another
/// caller is refused without naming that caller's child.
#[test]
fn request_key_covers_inputs_and_hides_other_callers_children() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let producer = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(
        tree.node(&producer.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.record_turn_completed(&producer.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &producer.agent_id);
    let mut keyed = mint_req();
    keyed.request_key = Some("k1".into());
    keyed.inputs = Some(vec![candidate_id.to_string()]);
    let first = tree.spawn_agent("root", &keyed, None).unwrap();
    let repeat = tree.spawn_agent("root", &keyed, None).unwrap();
    assert_eq!(repeat.agent_id, first.agent_id, "same brief replays");
    let mut without_inputs = keyed.clone();
    without_inputs.inputs = None;
    let error = tree.spawn_agent("root", &without_inputs, None).unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::RequestKeyConflict);
    // JJ uses a separate working-copy commit; later rewrites and candidate
    // retirement must not mutate the original admitted input identity.
    let workspace_id = tree.node(&first.agent_id).unwrap().workspace_id.clone();
    tree.workspaces.get_mut(&workspace_id).unwrap().base_commit =
        Some("different-working-copy".into());
    tree.candidates.remove(&candidate_id);
    assert_eq!(
        tree.spawn_agent("root", &keyed, None).unwrap().agent_id,
        first.agent_id
    );
    assert_eq!(
        tree.spawn_agent("root", &without_inputs, None)
            .unwrap_err()
            .code,
        TopologyErrorCode::RequestKeyConflict
    );
    // Another caller reusing the key learns only that it is spent.
    tree.grants.get_mut("root").unwrap().max_depth = 2;
    let hub = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    let mut hub_binding = RosterBinding::leaf("test-account", "selected-tuple");
    hub_binding.spawns = vec![TemplateName::parse("review").unwrap()];
    tree.pin_template("orchestrate", hub_binding);
    tree.grants.get_mut(&hub.agent_id).unwrap().may_spawn = true;
    let mut from_hub = spawn_req();
    from_hub.request_key = Some("k1".into());
    let error = tree
        .spawn_agent(&hub.agent_id, &from_hub, None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::RequestKeyConflict);
    assert!(
        !error.message.contains(&first.agent_id),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("another caller"),
        "{}",
        error.message
    );
}
