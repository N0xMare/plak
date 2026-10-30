#![allow(
    clippy::too_many_lines,
    reason = "candidate scenarios retain base pins, conflict lineage and adoption assertions across complete transitions"
)]

use super::SEAL_OUTCOME_KIND;
use super::merge_conflict_message;
use crate::tree::test_support::*;
use crate::tree::vcs_fixtures::*;
use plakous_workspace::occupancy_vcs::ConflictPath;

/// A long conflict list stops on a whole entry and names how many remain.
#[test]
fn conflict_message_cuts_whole_entries_and_says_plus_n() {
    let child = Path::new("/tmp/child-copy");
    let paths: Vec<ConflictPath> = (0..40)
        .map(|index| ConflictPath {
            path: format!("src/file-{index:02}.rs"),
            kind: if index == 1 {
                "binary".to_owned()
            } else {
                "contents".to_owned()
            },
        })
        .collect();
    let message = merge_conflict_message(&paths, child);
    let bytes = message.len();
    assert!(bytes <= 512, "{bytes} bytes: {message}");
    assert!(
        message.starts_with("merge_conflict in 40 path(s): "),
        "{message}"
    );
    assert!(
        message.ends_with(
            "; the child copy at /tmp/child-copy is retained untouched and nothing was applied"
        ),
        "{message}"
    );
    let listed = message
        .split_once(" path(s): ")
        .unwrap()
        .1
        .split_once("; the child copy at ")
        .unwrap()
        .0;
    let (shown, omitted) = listed.rsplit_once(", +").unwrap();
    let omitted: usize = omitted.strip_suffix(" more").unwrap().parse().unwrap();
    let shown: Vec<&str> = shown.split(", ").collect();
    assert_eq!(shown.len() + omitted, 40, "{listed}");
    assert!(shown.len() >= 2, "{listed}");
    assert_eq!(shown[0], "src/file-00.rs (contents)");
    assert_eq!(shown[1], "src/file-01.rs (binary)");
    for entry in &shown {
        assert!(
            entry.ends_with(" (contents)") || entry.ends_with(" (binary)"),
            "cut inside an entry: {entry}"
        );
    }
    let short = merge_conflict_message(&paths[..2], child);
    assert!(short.len() <= 512);
    assert!(!short.contains("more"), "{short}");
    assert!(
        short.contains("src/file-00.rs (contents), src/file-01.rs (binary)"),
        "{short}"
    );
}

#[test]
fn conflict_message_deduplicates_and_reserves_the_omission_marker_for_a_long_child_path() {
    let conflict = ConflictPath {
        path: " leading and trailing ".into(),
        kind: "contents".into(),
    };
    let paths = vec![conflict.clone(), conflict];
    let message = merge_conflict_message(&paths, Path::new("copy"));
    assert!(
        message.starts_with("merge_conflict in 1 path(s):  leading and trailing  (contents)"),
        "{message}"
    );
    let message = merge_conflict_message(&paths, Path::new(&"é".repeat(600)));
    assert!(message.len() <= 512, "{message}");
    assert!(message.contains("+1 more"), "{message}");
    assert!(
        message.ends_with("is retained untouched and nothing was applied"),
        "{message}"
    );
}

#[test]
fn jj_off_base_followup_seals_and_replays_the_whole_tree_notice() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let root_parent = Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
        .args(["log", "-r", "@-", "--no-graph", "-T", "commit_id"])
        .env("JJ_CONFIG", "/dev/null")
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(root_parent.status.success());
    let root_parent = String::from_utf8(root_parent.stdout).unwrap();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    let base = tree
        .workspaces
        .get(&child.workspace.id)
        .unwrap()
        .base_commit
        .clone()
        .unwrap();
    let shim = path.join(".plak/bin/jj");
    let output = Command::new(&shim)
        .args(["new", root_parent.trim()])
        .env("JJ_CONFIG", "/dev/null")
        .current_dir(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(plakous_workspace::git_redirect_fence::install_for_copy(&path, Some(&base)).is_ok());
    std::fs::write(path.join("off-base.txt"), "whole tree").unwrap();
    tree.submit_result(
        &child.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let notice = "history left plak_base(); the candidate is the whole tree";
    let result = tree
        .messages
        .values()
        .find(|mail| mail.purpose == MessagePurpose::Result)
        .unwrap();
    assert!(result.body.contains(notice), "{}", result.body);
    let outcome = tree
        .operations
        .values()
        .find(|operation| operation.tool_name == SEAL_OUTCOME_KIND)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&outcome.canonical_request).unwrap()["history_left_base"],
        true
    );
    let persisted = tree.to_persisted();
    let mut replay = ProductAgentTree::reopen(persisted);
    assert!(
        replay
            .messages
            .values()
            .any(|mail| mail.purpose == MessagePurpose::Result && mail.body.contains(notice))
    );
    let prior = replay.messages.len();
    replay.enqueue_result_mail(&child.agent_id);
    assert_eq!(replay.messages.len(), prior);
    replay
        .messages
        .retain(|_, mail| mail.purpose != MessagePurpose::Result);
    let attempt = replay
        .current_attempt(&child.agent_id)
        .unwrap()
        .attempt_id
        .clone();
    replay
        .operations
        .remove(&format!("plak:result:{}:{attempt}", child.agent_id));
    replay.enqueue_result_mail(&child.agent_id);
    assert!(
        replay
            .messages
            .values()
            .any(|mail| mail.purpose == MessagePurpose::Result && mail.body.contains(notice))
    );
    tree.send_agent_message(
        "root",
        &SendAgentMessageRequest {
            refresh: None,
            agent_id: child.agent_id.clone(),
            message: "follow up".into(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Running
    );
    tree.settle_for_test();
    assert!(plakous_workspace::git_redirect_fence::install_for_copy(&path, Some(&base)).is_ok());
    assert_eq!(
        std::fs::read_to_string(path.join("off-base.txt")).unwrap(),
        "whole tree"
    );
}

#[test]
fn failed_empty_candidate_unpin_is_recovery_and_cannot_discard_retained_pin() {
    let (_root, vcs, mut tree) = fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    vcs.seal_next_with(Ok(FakeVcs::base_for(&child.agent_id)));
    vcs.unpin_next_with(Err(VcsError::seal_failed(
        "injected private pin removal failure",
    )));
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(
        tree.attempt(&child.agent_id)
            .unwrap()
            .candidate_id
            .is_none()
    );
    assert!(
        tree.fault_reason(&child.agent_id)
            .unwrap()
            .contains("private pin removal failure")
    );
    let request = WorkspaceIdRequest {
        workspace_id: child.workspace.id,
    };
    assert_eq!(
        tree.discard_workspace("root", &request, None)
            .unwrap_err()
            .code,
        TopologyErrorCode::WorkspaceRecoveryRequired
    );
    assert_eq!(vcs.calls_of(FakeOp::Unpin).len(), 1);
    assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
}

/// A closed child's copy is kept while its candidate is unadopted, and
/// removed once the candidate lands; a copy that sealed nothing goes as
/// soon as it is closed.
#[test]
fn closed_copies_are_pruned_once_nothing_needs_them() {
    let (_root, vcs, mut tree) = fake_tree();
    let worker = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.record_turn_completed(&worker.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &worker.agent_id);
    tree.close_agent("root", &close_req(&worker.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert!(
        vcs.calls_of(FakeOp::Unmint).is_empty(),
        "an unadopted candidate keeps its copy"
    );
    assert_eq!(
        tree.workspace(&worker.workspace.id).unwrap().status,
        WorkspaceStatus::Active
    );
    let unpins_before = vcs.calls_of(FakeOp::Unpin).len();
    let adopted = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();
    assert_eq!(
        tree.candidates()[&candidate_id].merged_at.as_deref(),
        Some(adopted.target_after.split(':').next().unwrap())
    );
    tree.settle_for_test();
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
    assert_eq!(
        tree.workspace(&worker.workspace.id).unwrap().status,
        WorkspaceStatus::Discarded
    );
    // Copy removal drops only the mint base. Candidate and provenance
    // pins belong to the retained record, not the checkout.
    let ops: Vec<FakeOp> = vcs.calls().into_iter().map(|call| call.op).collect();
    assert_eq!(vcs.calls_of(FakeOp::Unpin).len(), unpins_before);
    assert_eq!(ops.last(), Some(&FakeOp::Unmint));
    let mut over = mint_req();
    over.inputs = Some(vec![candidate_id.to_string()]);
    tree.spawn_agent("root", &over, None).unwrap();

    let idle = tree.spawn_agent("root", &mint_req(), None).unwrap();
    vcs.seal_next_with(Ok(FakeVcs::base_for(&idle.agent_id)));
    tree.close_agent("root", &close_req(&idle.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 2);
    assert_eq!(
        tree.workspace(&idle.workspace.id).unwrap().status,
        WorkspaceStatus::Discarded
    );
}

/// A prune that fails leaves the closed copy and its node as they were
/// and is not retried in a loop.
#[test]
fn a_failed_prune_keeps_the_copy_without_recovery() {
    let (_root, vcs, mut tree) = fake_tree();
    let idle = tree.spawn_agent("root", &mint_req(), None).unwrap();
    vcs.seal_next_with(Ok(FakeVcs::base_for(&idle.agent_id)));
    vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected removal failure")));
    tree.close_agent("root", &close_req(&idle.agent_id), None)
        .unwrap();
    let settled = tree.settle_for_test();
    assert_eq!(settled.cleanup_failures, [] as [TopologyError; 0]);
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
    assert_eq!(
        tree.workspace(&idle.workspace.id).unwrap().status,
        WorkspaceStatus::Active
    );
    assert_eq!(
        tree.node(&idle.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    tree.close_agent("root", &close_req(&idle.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        vcs.calls_of(FakeOp::Unmint).len(),
        1,
        "not retried in this process"
    );
}

/// The seal and the adopt go through the `VcsOps` port: on `FakeVcs` a
/// closed copy seals the fake's snapshot as its candidate, a seal that
/// does not diverge records none and unpins, and an adopt lands the
/// fake's receipt.
#[test]
fn fake_vcs_seals_and_adopts_through_the_port() {
    let (_root, vcs, mut tree) = fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    assert_eq!(
        candidate_id,
        CandidateId::from_persisted(format!("cand-{}", child.attempt_id))
    );
    let sealed_commit = tree.candidates().get(&candidate_id).unwrap().commit.clone();
    assert_eq!(sealed_commit, format!("sealed-{candidate_id}"));
    assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
    let outcome = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();
    assert_eq!(outcome.target_after, sealed_commit);
    assert_eq!(outcome.files_changed, 1);
    assert_eq!(vcs.calls_of(FakeOp::Adopt).len(), 1);

    // A copy that never diverged: the fake answers the base itself.
    let second = tree.spawn_agent("root", &mint_req(), None).unwrap();
    vcs.seal_next_with(Ok(FakeVcs::base_for(&second.agent_id)));
    tree.record_turn_completed(&second.agent_id).unwrap();
    tree.settle_for_test();
    assert!(
        tree.attempt(&second.agent_id)
            .unwrap()
            .candidate_id
            .is_none()
    );
    assert_eq!(vcs.calls_of(FakeOp::Unpin).len(), 1);

    // A seal that could not run is a fault, not "nothing diverged".
    let third = tree.spawn_agent("root", &mint_req(), None).unwrap();
    vcs.seal_next_with(Err(VcsError::seal_failed("fake refused the pin")));
    tree.record_turn_completed(&third.agent_id).unwrap();
    tree.settle_for_test();
    let attempt = tree.attempt(&third.agent_id).unwrap();
    assert_eq!(attempt.status, AttemptStatus::RecoveryRequired);
    assert_eq!(
        attempt
            .failure
            .as_ref()
            .map(|failure| failure.kind.as_str()),
        Some(ProductAgentTree::SEAL_FAILED_KIND)
    );
    assert!(
        tree.fault_reason(&third.agent_id)
            .is_some_and(|reason| reason.contains("fake refused the pin"))
    );
}

/// An adopt the port refuses restores the row and reports the port's
/// reason under the matching wire code, without a repo.
#[test]
fn fake_vcs_adopt_failures_map_onto_wire_codes() {
    let (_root, vcs, mut tree) = fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let workspace_id = tree.node(&child.agent_id).unwrap().workspace_id.clone();
    for (failure, code) in [
        (
            AdoptFailure::Conflict {
                paths: vec![ConflictPath {
                    path: "a.rs".to_owned(),
                    kind: "contents".to_owned(),
                }],
                candidate: None,
            },
            TopologyErrorCode::MergeConflict,
        ),
        (
            AdoptFailure::TargetChanged("moved".to_owned()),
            TopologyErrorCode::TargetChanged,
        ),
        (
            AdoptFailure::Unavailable("no jj".to_owned()),
            TopologyErrorCode::MintUnavailable,
        ),
    ] {
        vcs.adopt_next_with(Err(failure));
        let refused = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap_err();
        assert_eq!(refused.code, code);
        assert_eq!(
            tree.workspace(&workspace_id).unwrap().status,
            WorkspaceStatus::Active,
            "the refused adopt restored the row"
        );
    }
    assert_eq!(vcs.calls_of(FakeOp::Adopt).len(), 3);
    assert!(
        tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
            .is_ok()
    );
}
use crate::tree::*;

#[test]
fn candidate_cleanup_cannot_unpin_another_trees_adoptable_work() {
    let repo = git_repo();
    let mut first = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let mut second = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    second.tree_id = "second-tree".to_owned();
    let a = first.spawn_agent("root", &mint_req(), None).unwrap();
    let b = second.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(first.node(&a.agent_id).unwrap().path.join("A.txt"), "a\n").unwrap();
    std::fs::write(second.node(&b.agent_id).unwrap().path.join("B.txt"), "b\n").unwrap();
    first.record_turn_completed(&a.agent_id).unwrap();
    second.record_turn_completed(&b.agent_id).unwrap();
    first.settle_for_test();
    second.settle_for_test();
    let first_candidate = candidate_of(&first, &a.agent_id);
    let second_candidate = candidate_of(&second, &b.agent_id);
    assert_eq!(first_candidate, second_candidate);
    assert_eq!(first_candidate.as_str(), "cand-c1-a1");
    first
        .close_agent("root", &close_req(&a.agent_id), None)
        .unwrap();
    first.settle_for_test();
    let adopted = first
        .adopt_candidate("root", &adopt_req(&first_candidate), None)
        .unwrap();
    first.settle_for_test();
    assert_eq!(
        first.candidates()[&first_candidate].merged_at.as_deref(),
        Some(adopted.target_after.split(':').next().unwrap())
    );
    assert!(!first.node(&a.agent_id).unwrap().path.exists());
    assert!(second.node(&b.agent_id).unwrap().path.exists());
    let listed = first
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_completed: Some(true),
                include_self: None,
                include_read: None,
            },
        )
        .unwrap();
    assert_eq!(
        listed.agents[0]["merged_at"],
        adopted.target_after.split(':').next().unwrap()
    );
    let reference = plakous_workspace::occupancy_vcs::candidate_ref(
        &digest_text(&second.tree_id),
        second_candidate.as_str(),
    );
    assert!(
        Command::new("git")
            .args(["show-ref", "--verify", "--quiet", &reference])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn candidate_inputs_are_refused_input_not_sealed() {
    let (_tmp, mut tree) = tree();
    let mut request = mint_req();
    request.inputs = Some(vec!["cand-c1-1".into()]);
    let error = tree.spawn_agent("root", &request, None).unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::InputNotSealed);
    assert_eq!(tree.nodes().len(), 1);
    // read_only with an input is a shape error, before anything else runs.
    let mut read_only = spawn_req();
    read_only.inputs = Some(vec!["cand-c1-1".into()]);
    assert_eq!(
        tree.spawn_agent("root", &read_only, None).unwrap_err().code,
        TopologyErrorCode::SchemaMismatch
    );
}

/// Git lands a candidate while a child minted over it is still running.
/// The landing writes the caller's tree only; the child copy is not rewritten.
#[test]
fn git_adopt_proceeds_while_a_child_minted_over_the_candidate_runs() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let first_path = tree.node(&first.agent_id).unwrap().path.clone();
    std::fs::write(first_path.join("H.txt"), "first pass\n").unwrap();
    tree.record_turn_completed(&first.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &first.agent_id);

    let mut over = mint_req();
    over.inputs = Some(vec![candidate_id.to_string()]);
    let second = tree.spawn_agent("root", &over, None).unwrap();
    let second_path = tree.node(&second.agent_id).unwrap().path.clone();
    std::fs::write(second_path.join("CHILD.txt"), "still running\n").unwrap();
    assert_eq!(
        tree.node(&second.agent_id).unwrap().status,
        NodeStatus::Running
    );

    tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
        "first pass\n"
    );
    assert_eq!(
        std::fs::read_to_string(second_path.join("H.txt")).unwrap(),
        "first pass\n",
        "the running copy was not rewritten"
    );
    assert_eq!(
        std::fs::read_to_string(second_path.join("CHILD.txt")).unwrap(),
        "still running\n"
    );
    assert_eq!(
        tree.node(&second.agent_id).unwrap().status,
        NodeStatus::Running
    );
}

/// Seals an input candidate and a second candidate minted on it.
fn seal_stacked_pair(tree: &mut ProductAgentTree) -> (CandidateId, CandidateId) {
    let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let first_path = tree.node(&first.agent_id).unwrap().path.clone();
    std::fs::write(first_path.join("INPUT.txt"), "input\n").unwrap();
    tree.record_turn_completed(&first.agent_id).unwrap();
    tree.settle_for_test();
    let input = candidate_of(tree, &first.agent_id);

    let mut over = mint_req();
    over.inputs = Some(vec![input.to_string()]);
    let second = tree.spawn_agent("root", &over, None).unwrap();
    let second_path = tree.node(&second.agent_id).unwrap().path.clone();
    std::fs::write(second_path.join("STACKED.txt"), "stacked\n").unwrap();
    tree.record_turn_completed(&second.agent_id).unwrap();
    tree.settle_for_test();
    (input, candidate_of(tree, &second.agent_id))
}

#[test]
fn adopting_a_candidate_stacked_on_an_unadopted_candidate_is_refused_by_name() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let (input, stacked) = seal_stacked_pair(&mut tree);
    let refused = tree
        .adopt_candidate("root", &adopt_req(&stacked), None)
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::AdoptNotApplicable);
    assert!(
        refused.message.contains(stacked.as_str())
            && refused.message.contains(input.as_str())
            && refused.message.contains("is not adopted")
            && refused.message.contains("nothing was adopted"),
        "{}",
        refused.message
    );
    assert!(!repo.path().join("STACKED.txt").exists());
    assert!(!repo.path().join("INPUT.txt").exists());
}

#[test]
fn adopting_a_candidate_stacked_on_an_unadopted_candidate_passes_after_the_input_is_adopted() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let (input, stacked) = seal_stacked_pair(&mut tree);
    tree.adopt_candidate("root", &adopt_req(&input), None)
        .unwrap();
    tree.adopt_candidate("root", &adopt_req(&stacked), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("INPUT.txt")).unwrap(),
        "input\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("STACKED.txt")).unwrap(),
        "stacked\n"
    );
}

#[test]
fn stacked_input_adopted_by_an_inherit_hub_in_the_callers_checkout_passes() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let (hub, _child, _workspace, input) = hub_with_sealed_grandchild(&mut tree);
    tree.adopt_candidate(&hub, &adopt_req(&input), None)
        .unwrap();

    let mut over = mint_req();
    over.inputs = Some(vec![input.to_string()]);
    let stacked_node = tree.spawn_agent("root", &over, None).unwrap();
    let stacked_path = tree.node(&stacked_node.agent_id).unwrap().path.clone();
    std::fs::write(stacked_path.join("STACKED.txt"), "stacked\n").unwrap();
    tree.record_turn_completed(&stacked_node.agent_id).unwrap();
    tree.settle_for_test();
    let stacked = candidate_of(&tree, &stacked_node.agent_id);

    tree.adopt_candidate("root", &adopt_req(&stacked), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("ONE.txt")).unwrap(),
        "one\n",
        "the hub shares this checkout, so its adopt of the input already landed"
    );
    assert!(repo.path().join("STACKED.txt").exists());
}

fn test_hub(
    tree: &mut ProductAgentTree,
    parent: &str,
    name: &str,
    children: &[&str],
    fork: bool,
) -> String {
    for child in children {
        if !tree
            .snapshot
            .roster
            .contains_key(&TemplateName::parse(child).unwrap())
        {
            tree.pin_template(child, RosterBinding::leaf("test-account", "selected-tuple"));
        }
    }
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.spawns = children
        .iter()
        .map(|name| TemplateName::parse(name).unwrap())
        .collect();
    binding.may_adopt = true;
    tree.pin_template(name, binding);
    let mut request = SpawnAgentRequest::test_named(name, Tier::Mutate, "hub", "coordinate");
    if fork {
        request.workspace = SpawnWorkspace::Fork;
    }
    let hub = tree.spawn_agent(parent, &request, None).unwrap();
    tree.grants.get_mut(&hub.agent_id).unwrap().may_spawn = true;
    tree.grants.get_mut(&hub.agent_id).unwrap().may_adopt = true;
    hub.agent_id
}

#[test]
fn stacked_input_follows_nested_hub_carriers() {
    for inner_fork in [false, true] {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        tree.grants.get_mut("root").unwrap().max_depth = 3;
        let outer = test_hub(&mut tree, "root", "orchestrate", &["review"], true);
        let inner = test_hub(&mut tree, &outer, "review", &["implement"], inner_fork);
        let child = tree.spawn_agent(&inner, &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("INPUT.txt"),
            "input\n",
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let input = candidate_of(&tree, &child.agent_id);
        tree.adopt_candidate(&inner, &adopt_req(&input), None)
            .unwrap();
        let mut over = mint_req();
        over.inputs = Some(vec![input.to_string()]);
        let stacked = tree.spawn_agent("root", &over, None).unwrap();
        std::fs::write(
            tree.node(&stacked.agent_id)
                .unwrap()
                .path
                .join("STACKED.txt"),
            "stacked\n",
        )
        .unwrap();
        tree.record_turn_completed(&stacked.agent_id).unwrap();
        tree.settle_for_test();
        let stacked = candidate_of(&tree, &stacked.agent_id);
        let refused = tree
            .adopt_candidate("root", &adopt_req(&stacked), None)
            .unwrap_err();
        if !inner_fork {
            assert!(
                refused
                    .message
                    .contains(&format!("ask {inner} to adopt {stacked}")),
                "{}",
                refused.message
            );
        }
        tree.record_turn_completed(&inner).unwrap();
        tree.settle_for_test();
        if inner_fork {
            let carrier = candidate_of(&tree, &inner);
            tree.adopt_candidate(&outer, &adopt_req(&carrier), None)
                .unwrap();
        }
        tree.record_turn_completed(&outer).unwrap();
        tree.settle_for_test();
        let carrier = candidate_of(&tree, &outer);
        tree.adopt_candidate("root", &adopt_req(&carrier), None)
            .unwrap();
        tree.adopt_candidate("root", &adopt_req(&stacked), None)
            .unwrap();
        assert!(repo.path().join("INPUT.txt").exists());
        assert!(repo.path().join("STACKED.txt").exists());
    }
}

#[test]
fn stacked_input_passes_after_a_later_candidate_from_the_source_copy_lands() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    tree.grants.get_mut("root").unwrap().max_depth = 2;
    let hub = test_hub(&mut tree, "root", "orchestrate", &["implement"], true);
    let child = tree.spawn_agent(&hub, &mint_req(), None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(path.join("INPUT.txt"), "input\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let input = candidate_of(&tree, &child.agent_id);
    let mut over = mint_req();
    over.inputs = Some(vec![input.to_string()]);
    let stacked = tree.spawn_agent("root", &over, None).unwrap();
    std::fs::write(
        tree.node(&stacked.agent_id)
            .unwrap()
            .path
            .join("STACKED.txt"),
        "stacked\n",
    )
    .unwrap();
    tree.record_turn_completed(&stacked.agent_id).unwrap();
    tree.settle_for_test();
    let stacked = candidate_of(&tree, &stacked.agent_id);
    let mut followup = mail(&child.agent_id, "continue");
    followup.purpose = Some(MessagePurpose::Followup);
    tree.send_agent_message(&hub, &followup, None).unwrap();
    std::fs::write(path.join("LATER.txt"), "later\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let later = candidate_of(&tree, &child.agent_id);
    tree.adopt_candidate("root", &adopt_req(&later), None)
        .unwrap();
    tree.adopt_candidate(&hub, &adopt_req(&input), None)
        .unwrap();
    tree.adopt_candidate("root", &adopt_req(&stacked), None)
        .unwrap();
    assert!(repo.path().join("INPUT.txt").exists());
    assert!(repo.path().join("LATER.txt").exists());
    assert!(repo.path().join("STACKED.txt").exists());
}

/// The caller already landed the adopter's later candidate. The adopter is
/// a forked hub: an inherit hub shares the root checkout and seals nothing,
/// so it cannot be the later candidate the rule looks up.
#[test]
fn adopting_a_candidate_stacked_on_an_input_passes_once_the_adopters_candidate_landed() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    tree.grants.get_mut("root").unwrap().max_depth = 2;
    let mut hub_binding = RosterBinding::leaf("test-account", "selected-tuple");
    hub_binding.spawns = vec![TemplateName::parse("implement").unwrap()];
    hub_binding.may_adopt = true;
    tree.pin_template("orchestrate", hub_binding);
    let mut hub_request =
        SpawnAgentRequest::test_named("orchestrate", Tier::Mutate, "hub", "coordinate");
    hub_request.workspace = SpawnWorkspace::Fork;
    let hub = tree.spawn_agent("root", &hub_request, None).unwrap();
    tree.grants.get_mut(&hub.agent_id).unwrap().may_spawn = true;
    tree.grants.get_mut(&hub.agent_id).unwrap().may_adopt = true;
    let child = tree.spawn_agent(&hub.agent_id, &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("INPUT.txt"), "input\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let input = candidate_of(&tree, &child.agent_id);
    let hub_path = tree.node(&hub.agent_id).unwrap().path.clone();
    std::fs::write(hub_path.join("HUB.txt"), "hub\n").unwrap();
    tree.record_turn_completed(&hub.agent_id).unwrap();
    tree.settle_for_test();
    let hubs = candidate_of(&tree, &hub.agent_id);
    tree.adopt_candidate("root", &adopt_req(&hubs), None)
        .unwrap();

    let mut followup = mail(&hub.agent_id, "carry the input");
    followup.purpose = Some(MessagePurpose::Followup);
    tree.send_agent_message("root", &followup, None).unwrap();
    tree.adopt_candidate(&hub.agent_id, &adopt_req(&input), None)
        .unwrap();

    let mut over = mint_req();
    over.inputs = Some(vec![input.to_string()]);
    let stacked_node = tree.spawn_agent("root", &over, None).unwrap();
    let stacked_path = tree.node(&stacked_node.agent_id).unwrap().path.clone();
    std::fs::write(stacked_path.join("STACKED.txt"), "stacked\n").unwrap();
    tree.record_turn_completed(&stacked_node.agent_id).unwrap();
    tree.settle_for_test();
    let stacked = candidate_of(&tree, &stacked_node.agent_id);
    let refused = tree
        .adopt_candidate("root", &adopt_req(&stacked), None)
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::AdoptNotApplicable);
    assert!(!repo.path().join("INPUT.txt").exists());

    tree.record_turn_completed(&hub.agent_id).unwrap();
    tree.settle_for_test();
    let hubs = candidate_of(&tree, &hub.agent_id);
    tree.adopt_candidate("root", &adopt_req(&hubs), None)
        .unwrap();
    tree.adopt_candidate("root", &adopt_req(&stacked), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("STACKED.txt")).unwrap(),
        "stacked\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("INPUT.txt")).unwrap(),
        "input\n"
    );
}

#[test]
fn stacked_input_checks_all_jj_candidates_without_repository_probes() {
    let (root, vcs, mut tree) = fake_tree();
    let (input, stacked) = seal_stacked_pair(&mut tree);
    let candidate = tree.candidates[&stacked].clone();
    tree.workspaces
        .get_mut(&candidate.workspace_id)
        .unwrap()
        .path = root.path().join(".plak/workspaces/stacked");
    for index in 0..12 {
        let mut unrelated = tree.candidates[&input].clone();
        unrelated.candidate_id = CandidateId::from_persisted(format!("aaa-{index}"));
        unrelated.commit = format!("unrelated-{index}");
        tree.candidates
            .insert(unrelated.candidate_id.clone(), unrelated);
    }
    assert!(tree.stacked_input_blocker("root", &candidate).is_some());
    assert!(vcs.calls_of(FakeOp::OverCandidate).is_empty());
    for kind in ["worktrees", "workspaces"] {
        let mut same_copy = candidate.clone();
        same_copy.workspace_id = tree.candidates[&input].workspace_id.clone();
        tree.workspaces
            .get_mut(&same_copy.workspace_id)
            .unwrap()
            .path = root.path().join(".plak").join(kind).join("input");
        assert!(tree.stacked_input_blocker("root", &same_copy).is_none());
    }
}

#[test]
fn jj_stacked_input_uses_the_immutable_recorded_mint_base() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let (input, stacked) = seal_stacked_pair(&mut tree);
    let refused = tree
        .adopt_candidate("root", &adopt_req(&stacked), None)
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::AdoptNotApplicable);
    assert!(refused.message.contains(input.as_str()));
}

/// An input whose copy was explicitly given up is not a stacked blocker.
#[test]
fn adopting_a_candidate_stacked_on_a_discarded_input_passes() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let (input, stacked) = seal_stacked_pair(&mut tree);
    let workspace = tree.candidates()[&input].workspace_id.clone();
    tree.discard_workspace(
        "root",
        &WorkspaceIdRequest {
            workspace_id: workspace,
        },
        None,
    )
    .unwrap();
    tree.settle_for_test();
    tree.adopt_candidate("root", &adopt_req(&stacked), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("STACKED.txt")).unwrap(),
        "stacked\n"
    );
}

#[test]
fn automatically_reclaimed_input_is_still_a_stacked_blocker() {
    let (_root, _vcs, mut tree) = fake_tree();
    let (input, stacked) = seal_stacked_pair(&mut tree);
    let workspace_id = tree.candidates[&input].workspace_id.clone();
    tree.workspaces.get_mut(&workspace_id).unwrap().status = WorkspaceStatus::CleanRemoved;
    assert!(
        tree.stacked_input_blocker("root", &tree.candidates[&stacked])
            .is_some()
    );
    tree.workspaces.get_mut(&workspace_id).unwrap().status = WorkspaceStatus::Discarded;
    assert!(
        tree.stacked_input_blocker("root", &tree.candidates[&stacked])
            .is_none()
    );
}

/// an `inputs` entry starts the child from a sealed
/// candidate rather than from its parent's live state. The pipeline shape
/// — one agent's copy handed to the next — is exactly this.
#[test]
fn spawn_over_a_sealed_candidate_mints_from_it() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let first_path = tree.node(&first.agent_id).unwrap().path.clone();
    std::fs::write(first_path.join("H.txt"), "first pass\n").unwrap();
    tree.record_turn_completed(&first.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &first.agent_id);

    let mut over = mint_req();
    over.inputs = Some(vec![candidate_id.to_string()]);
    let second = tree.spawn_agent("root", &over, None).unwrap();
    let second_path = tree.node(&second.agent_id).unwrap().path.clone();
    assert_eq!(
        std::fs::read_to_string(second_path.join("H.txt")).unwrap(),
        "first pass\n",
        "the copy starts from the candidate, not from the parent"
    );
    assert!(
        !repo.path().join("H.txt").exists(),
        "handing a candidate to a child lands nothing in the human checkout"
    );
    assert_eq!(
        tree.workspace(&second.workspace.id)
            .unwrap()
            .base_commit
            .as_deref(),
        Some(
            tree.candidates()
                .get(&candidate_id)
                .unwrap()
                .commit
                .as_str()
        ),
        "the candidate is the merge base a later adopt reads"
    );

    // Two candidates would be a merge the spawn is not authorized to do.
    let mut two = mint_req();
    two.inputs = Some(vec![candidate_id.to_string(), "cand-other-a1".to_owned()]);
    assert_eq!(
        tree.spawn_agent("root", &two, None).unwrap_err().code,
        TopologyErrorCode::SchemaMismatch
    );
}

/// Authority is checked before anything else, and an unknown candidate id
/// is `not_sealed` with nothing adopted.
#[test]
fn adopt_candidate_checks_authority_then_refuses_not_sealed() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let request = AdoptCandidateRequest {
        revert: false,
        accept_conflicts: false,
        candidate_id: CandidateId::from_persisted("cand-c1-1".to_owned()),
        expected_target: None,
        evidence: None,
    };
    assert_eq!(
        tree.adopt_candidate(&spawned.agent_id, &request, None)
            .unwrap_err()
            .code,
        TopologyErrorCode::NotAuthorized
    );
    let error = tree
        .adopt_candidate("root", &request, Some("op-adopt"))
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::NotSealed);
    assert!(
        error.message.contains("nothing was adopted"),
        "{}",
        error.message
    );
    assert!(tree.candidates().is_empty());
    assert!(
        !tree
            .compiled_tool_names(&spawned.agent_id)
            .unwrap()
            .contains(&"adopt_candidate")
    );
    assert!(
        tree.compiled_tool_names("root")
            .unwrap()
            .contains(&"adopt_candidate")
    );
}

/// A quota refusal is not an ordinary failed turn: the
/// **attempt** carries the reason and the **node** goes `Failed`, because a
/// blocked attempt has no node status of its own. Nothing auto-retries and
/// no other account is spent.
#[test]
fn quota_block_seals_partial_and_does_not_retry() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.set_active_turn(&spawned.agent_id, true).unwrap();
    tree.record_attempt_blocked(
        &spawned.agent_id,
        BlockedReason {
            quota: "quota_exceeded".to_owned(),
            account: "codex".to_owned(),
            resets_at: Some(1_800_000_000),
        },
    )
    .unwrap();
    tree.settle_for_test();

    let attempt = tree.attempt(&spawned.agent_id).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Blocked);
    let blocked = attempt.blocked.clone().unwrap();
    assert_eq!(blocked.quota, "quota_exceeded");
    assert_eq!(blocked.account, "codex");
    assert_eq!(blocked.resets_at, Some(1_800_000_000));
    assert!(attempt.ended_at.is_some());

    let node = tree.node(&spawned.agent_id).unwrap();
    assert_eq!(node.status, NodeStatus::Failed);
    assert!(!node.active_turn);
    assert_eq!(
        tree.attempts()
            .values()
            .filter(|row| row.agent_id == spawned.agent_id)
            .count(),
        1,
        "nothing auto-retries"
    );
    assert!(
        tree.messages().values().any(|mail| {
            mail.purpose == MessagePurpose::Result && mail.body.contains("blocked")
        }),
        "the parent learns without polling"
    );
    assert!(
        tree.poll_wait("root", &wait_for(&spawned.agent_id))
            .unwrap()
            .is_some(),
        "a blocked attempt settles an open wait"
    );
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
    assert_eq!(listed.agents[0]["blocked"]["resets_at"], 1_800_000_000_u64);
}

/// The four termination paths all seal. Every `execute` and
/// `mutate` copy produces a candidate; `read_only` produces none.
#[test]
fn termination_paths_all_seal_a_candidate() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let completed = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let closed = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let failed = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let reader = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    for id in [&completed.agent_id, &closed.agent_id, &failed.agent_id] {
        std::fs::write(tree.node(id).unwrap().path.join("H.txt"), format!("{id}\n")).unwrap();
    }
    tree.record_turn_completed(&completed.agent_id).unwrap();
    tree.settle_for_test();
    tree.close_agent("root", &close_req(&closed.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    tree.record_host_materialization(&failed.agent_id)
        .and_then(|value| tree.fold_due(&failed.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();
    for (id, status) in [
        (&completed.agent_id, NodeStatus::Failed),
        (&closed.agent_id, NodeStatus::Closed),
        (&failed.agent_id, NodeStatus::RecoveryRequired),
    ] {
        assert_ne!(tree.node(id).unwrap().status, NodeStatus::Sealing, "{id}");
        assert_eq!(tree.node(id).unwrap().status, status, "{id}");
        let candidate = tree
            .attempt(id)
            .unwrap()
            .candidate_id
            .clone()
            .unwrap_or_else(|| unreachable!("{id} sealed"));
        let record = tree.candidates().get(&candidate).unwrap();
        assert_eq!(record.producing_agent_id, *id);
        assert_eq!(record.commit.len(), 40, "{}", record.commit);
        assert!(record.adopted_by.is_none());
    }
    // A read_only node has nothing of its own to seal.
    assert!(!tree.begin_seal(&reader.agent_id));
    assert!(
        tree.attempt(&reader.agent_id)
            .unwrap()
            .candidate_id
            .is_none()
    );
}

/// the snapshot MUST run with the tree mutex released. The
/// mutation marks the node and queues the ticket under the lock; the VCS
/// half runs on the effects batch, which holds no tree reference.
#[test]
fn seal_runs_outside_the_tree_mutex() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(
        tree.node(&spawned.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.record_turn_completed(&spawned.agent_id).unwrap();
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Sealing
    );
    assert!(tree.candidates().is_empty());
    let pending = tree.take_effects();
    assert_eq!(pending.seals.len(), 1);
    // The VCS half needs nothing from the tree.
    let completed = pending.run(&SystemVcs);
    tree.complete(completed);
    assert_eq!(tree.candidates().len(), 1);
    // The turn ended without a submit, so the status the termination path
    // chose is restored — not `Completed` blindly.
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Failed
    );
    assert_eq!(tree.candidates().len(), 1);
}

/// A dirty parent (modified, untracked, deleted, and renamed paths) forks a
/// child whose copy *is* the parent's state, seals it, and adopts it back
/// without disturbing the parent's dirty files or its HEAD.
#[test]
fn dirty_parent_fork_seals_and_adopts() {
    let repo = git_repo();
    std::fs::write(repo.path().join("DEL.txt"), "del\n").unwrap();
    std::fs::write(repo.path().join("OLD.txt"), "old\n").unwrap();
    git_commit(repo.path(), &["DEL.txt", "OLD.txt"], "more");
    // Modified, untracked, deleted, renamed — the four shapes the deleted
    // `copy_dirty_files` parser mishandled.
    std::fs::write(repo.path().join("SEED.txt"), "changed\n").unwrap();
    std::fs::write(repo.path().join("F.txt"), "dirty-F\n").unwrap();
    std::fs::remove_file(repo.path().join("DEL.txt")).unwrap();
    std::fs::rename(repo.path().join("OLD.txt"), repo.path().join("NEW.txt")).unwrap();
    let parent_status = git_porcelain(repo.path());
    let parent_head = git_head(repo.path());

    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();

    // The copy is the parent's state, not the launch HEAD plus a file copy.
    assert_eq!(
        std::fs::read_to_string(child_path.join("SEED.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        std::fs::read_to_string(child_path.join("F.txt")).unwrap(),
        "dirty-F\n"
    );
    assert!(!child_path.join("DEL.txt").exists(), "deletion travelled");
    assert!(!child_path.join("OLD.txt").exists(), "rename travelled");
    assert!(child_path.join("NEW.txt").is_file(), "rename travelled");
    let base = tree
        .workspace(&child.workspace.id)
        .unwrap()
        .base_commit
        .clone()
        .unwrap_or_else(|| unreachable!("mint records a base"));
    assert_eq!(base.len(), 40);
    // The parent is untouched by the mint.
    assert_eq!(git_porcelain(repo.path()), parent_status);
    assert_eq!(git_head(repo.path()), parent_head);

    std::fs::write(child_path.join("H.txt"), "child work\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);

    let outcome = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), Some("op-adopt"))
        .unwrap();
    assert_eq!(outcome.candidate_id, candidate_id);
    assert_eq!(outcome.files_changed, 1);
    assert_ne!(outcome.target_before, outcome.target_after);
    // The child's file landed; the human's dirty files are intact and the
    // human checkout received no commit.
    assert_eq!(
        std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
        "child work\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("F.txt")).unwrap(),
        "dirty-F\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
        "changed\n"
    );
    assert!(!repo.path().join("DEL.txt").exists());
    assert_eq!(git_head(repo.path()), parent_head, "no commit in the root");
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Adopted
    );
    assert_eq!(
        tree.candidates()
            .get(&candidate_id)
            .unwrap()
            .adopted_by
            .as_deref(),
        Some("root")
    );
    assert_eq!(
        tree.attempt(&child.agent_id).unwrap().status,
        AttemptStatus::Adopted
    );
    // Replay is the recorded receipt, not a second landing.
    let replay = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), Some("op-adopt"))
        .unwrap();
    assert_eq!(replay, outcome);
}

/// A human edit made in the parent *after* the mint survives the
/// adopt, and a stale `expected_target` refuses with nothing applied.
#[test]
fn adopt_refuses_changed_target_and_preserves_unrelated_edits() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    let stale = tree
        .workspace(&child.workspace.id)
        .unwrap()
        .base_commit
        .clone()
        .unwrap();
    // The human keeps working in the root while the child runs.
    std::fs::write(repo.path().join("HUMAN.txt"), "human\n").unwrap();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);

    let mut stale_request = adopt_req(&candidate_id);
    stale_request.expected_target = Some(stale);
    let error = tree
        .adopt_candidate("root", &stale_request, None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::TargetChanged);
    assert!(error.message.contains("nothing was applied"), "{error:?}");
    assert!(!repo.path().join("H.txt").exists(), "nothing was applied");
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Active,
        "a refused adopt restores the row"
    );

    let outcome = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();
    assert_eq!(outcome.files_changed, 1);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
        "child\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("HUMAN.txt")).unwrap(),
        "human\n",
        "the human edit made after the mint survives"
    );
}

/// A fresh `expected_target` is accepted; it is the snapshot token, the
/// commit, or the tree id.
#[test]
fn adopt_accepts_a_current_expected_target() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let current = plakous_workspace::occupancy_vcs::snapshot(repo.path(), "probe").unwrap();
    let mut request = adopt_req(&candidate_id);
    // The tree id is the durable content identity; the commit wrapping it
    // also carries the snapshot's label, so quote the content.
    request.expected_target = Some(current.content.clone());
    assert_eq!(
        tree.adopt_candidate("root", &request, None)
            .unwrap()
            .files_changed,
        1
    );
}

/// A conflicting adopt is a failed result, never a success status: the
/// parent keeps its own content and the child copy is retained.
#[test]
fn adopt_conflict_retains_child_copy() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("SEED.txt"), "child side\n").unwrap();
    std::fs::write(repo.path().join("SEED.txt"), "human side\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let error = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::MergeConflict);
    assert!(error.message.contains("SEED.txt"), "{error:?}");
    assert!(error.message.contains("retained"), "{error:?}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
        "human side\n",
        "the parent is untouched"
    );
    assert_eq!(
        std::fs::read_to_string(child_path.join("SEED.txt")).unwrap(),
        "child side\n",
        "the child copy is retained"
    );
    assert!(
        tree.candidates()
            .get(&candidate_id)
            .unwrap()
            .adopted_by
            .is_none()
    );
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Active
    );
}

/// Everything the child wrote before its turn ended is in the
/// candidate; nothing it writes after the seal is.
#[test]
fn candidate_holds_writes_until_the_turn_ends_and_no_later() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("BEFORE.txt"), "1\n").unwrap();
    tree.submit_result(
        &child.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    // the child can still write until the turn ends.
    std::fs::write(child_path.join("AFTER_SUBMIT.txt"), "2\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    std::fs::write(child_path.join("AFTER_SEAL.txt"), "3\n").unwrap();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let content = tree
        .candidates()
        .get(&candidate_id)
        .unwrap()
        .content
        .clone();
    let paths = git_tree_paths(repo.path(), &content);
    assert!(paths.iter().any(|p| p == "BEFORE.txt"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "AFTER_SUBMIT.txt"), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p == "AFTER_SEAL.txt"),
        "a write after the seal is not in the candidate: {paths:?}"
    );
}

/// L4: `close_agent` on a running child still seals, and the candidate
/// survives the close.
#[test]
fn close_agent_keeps_candidate_until_explicit_discard() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    tree.close_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let record = tree.candidates().get(&candidate_id).unwrap().clone();
    assert!(record.adopted_by.is_none());
    assert_eq!(record.commit.len(), 40);
    // Close retains the candidate until its manager explicitly gives it up.
    let request = WorkspaceIdRequest {
        workspace_id: child.workspace.id.clone(),
    };
    assert_eq!(
        tree.discard_workspace("root", &request, None)
            .unwrap()
            .status,
        WorkspaceStatus::Discarding
    );
    tree.settle_for_test();
}

/// The `refs/plak/<namespace>/candidates/<id>` pin outlives the adopt (a later adopt
/// of the same copy needs it as its merge base) and belongs to the record,
/// not the copy's lifetime.
#[test]
fn candidate_ref_survives_removal_of_its_copy() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let commit = tree.candidates().get(&candidate_id).unwrap().commit.clone();
    tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();
    let reference = plakous_workspace::occupancy_vcs::candidate_ref(
        &digest_text(&tree.tree_id),
        candidate_id.as_str(),
    );
    let resolve = || {
        Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", reference.as_str()])
            .current_dir(repo.path())
            .output()
            .unwrap()
    };
    let pinned = resolve();
    assert!(pinned.status.success(), "the pin is gone after adopt");
    assert_eq!(String::from_utf8_lossy(&pinned.stdout).trim(), commit);
    tree.discard_workspace(
        "root",
        &WorkspaceIdRequest {
            workspace_id: child.workspace.id.clone(),
        },
        None,
    )
    .unwrap();
    tree.settle_for_test();
    assert!(!child_path.exists(), "the copy is gone");
    assert!(
        resolve().status.success(),
        "copy removal lost a retained candidate pin"
    );
}

/// Adoption authority is subtree membership plus `may_adopt`, and evidence
/// must name attempts that exist in that subtree.
#[test]
fn review_evidence_names_the_exact_candidate() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let reviewer = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    tree.record_turn_completed(&reviewer.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);

    // A sibling with no adopt grant cannot land it.
    assert_eq!(
        tree.adopt_candidate(&reviewer.agent_id, &adopt_req(&candidate_id), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::NotAuthorized
    );
    let mut bogus = adopt_req(&candidate_id);
    bogus.evidence = Some(vec!["nope-a9".into()]);
    assert_eq!(
        tree.adopt_candidate("root", &bogus, None).unwrap_err().code,
        TopologyErrorCode::EvidenceMismatch
    );
    let mut evidenced = adopt_req(&candidate_id);
    evidenced.evidence = Some(vec![reviewer.attempt_id.to_string()]);
    assert_eq!(
        tree.adopt_candidate("root", &evidenced, None)
            .unwrap()
            .candidate_id,
        candidate_id
    );
}

/// The jj mirror of base, seal, adopt, and conflict.
#[test]
fn jj_mint_seals_and_adopts_a_candidate() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    std::fs::write(repo.path().join("DIRTY.txt"), "dirty\n").unwrap();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    assert!(child_path.join("DIRTY.txt").is_file(), "the base travelled");
    let workspace = tree.workspace(&child.workspace.id).unwrap();
    assert!(
        workspace
            .base_commit
            .as_ref()
            .is_some_and(|id| !id.is_empty()),
        "the jj mint records a base"
    );
    // No commit: jj auto-snapshots the working copy into `@`, which is
    // exactly what the seal captures.
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let record = tree.candidates().get(&candidate_id).unwrap().clone();
    assert!(record.change_id.is_some(), "jj pins the change id");
    let outcome = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();
    assert_eq!(outcome.candidate_id, candidate_id);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
        "child\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("DIRTY.txt")).unwrap(),
        "dirty\n",
        "the human's dirty file is intact"
    );
}

/// Gate 3: any jj command anywhere in the repo — here the parent's own
/// `jj status`, but a sibling mint or an adopt does the same — leaves the
/// child's copy stale, and a stale copy refuses `status` and `log`. The
/// seal used to swallow that and report `candidate: null` while the work
/// sat in the copy.
#[test]
fn jj_seal_after_parent_activity_still_produces_a_candidate() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    jj_in(repo.path(), &["status"]);
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    assert_ne!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired,
        "a stale copy is recovered, not a fault"
    );
    assert!(tree.fault_reason(&child.agent_id).is_none());
    tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
        "child\n"
    );
}

/// The other half of the same fix: recovering the stale copy must not
/// invent divergence either. The child wrote nothing, so there is nothing
/// to keep even though its `@` commit id moved under it.
#[test]
fn jj_untouched_copy_after_parent_activity_seals_nothing() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(repo.path().join("HUMAN.txt"), "human\n").unwrap();
    jj_in(repo.path(), &["status"]);
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert!(
        tree.attempt(&child.agent_id)
            .unwrap()
            .candidate_id
            .is_none(),
        "a copy that wrote nothing has nothing to keep"
    );
    assert!(tree.candidates().is_empty(), "{:?}", tree.candidates());
}

/// A seal that could not *run* is a fault, not "nothing diverged".
/// Reported on git, where the copy is simply gone.
#[test]
fn a_seal_that_cannot_run_leaves_the_node_recovery_required() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    std::fs::remove_dir_all(&child_path).unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    let attempt = tree.attempt(&child.agent_id).unwrap();
    assert!(attempt.candidate_id.is_none());
    let failure = attempt.failure.as_ref().unwrap();
    assert_eq!(failure.kind, ProductAgentTree::SEAL_FAILED_KIND);
    assert!(
        failure.message.contains("could not be sealed"),
        "{}",
        failure.message
    );
    assert!(
        tree.fault_reason(&child.agent_id)
            .is_some_and(|reason| reason.contains("could not be sealed")),
        "the fault reason is the only account of a failed seal"
    );
    assert!(
        tree.candidates().is_empty(),
        "a failed seal records no candidate at all, null or otherwise"
    );
}

/// A `mutate` child minted over an *unadopted* candidate is stacked on
/// it. Adopting the child would land only the child's delta and leave
/// the input's changes unadopted, so the landing is refused.
#[test]
fn jj_child_minted_over_an_unadopted_candidate_is_refused() {
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
    assert_eq!(
        std::fs::read_to_string(second_path.join("FIRST.txt")).unwrap(),
        "first\n",
        "the copy starts from the candidate"
    );
    std::fs::write(second_path.join("SECOND.txt"), "second\n").unwrap();
    tree.record_turn_completed(&second.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &second.agent_id);
    let refused = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::AdoptNotApplicable);
    assert!(
        refused.message.contains(candidate_id.as_str())
            && refused.message.contains(input.as_str())
            && refused.message.contains("is not adopted")
            && refused.message.contains("nothing was adopted"),
        "{}",
        refused.message
    );
    assert!(!repo.path().join("SECOND.txt").exists());
    assert!(
        !repo.path().join("FIRST.txt").exists(),
        "the refused landing did not take the input's delta either"
    );
    assert_eq!(
        std::fs::read_to_string(first_path.join("FIRST.txt")).unwrap(),
        "first\n",
        "the sibling's own working copy is intact"
    );
    assert!(
        tree.candidates().get(&input).unwrap().adopted_by.is_none(),
        "the input candidate was not adopted by this landing"
    );
}

/// Two `mutate` siblings minted from the same
/// parent working copy, both sealed, adopted one after the other. The
/// first adopt rewrites the parent's `@`, which rebases the second
/// sibling's working-copy commit under it and leaves that copy stale; the
/// adopt's base probe ran plain `jj log` *inside* that copy, got "The
/// working copy is stale", read the base as gone, and refused the second
/// adopt `mint_unavailable` — its file never landed.
#[test]
fn jj_second_sibling_still_adopts_after_the_first_landed() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let second = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let first_path = tree.node(&first.agent_id).unwrap().path.clone();
    let second_path = tree.node(&second.agent_id).unwrap().path.clone();
    std::fs::write(first_path.join("ONE.md"), "one\n").unwrap();
    std::fs::write(second_path.join("OTHER.md"), "other\n").unwrap();
    tree.record_turn_completed(&first.agent_id).unwrap();
    tree.settle_for_test();
    tree.record_turn_completed(&second.agent_id).unwrap();
    tree.settle_for_test();
    let first_candidate = candidate_of(&tree, &first.agent_id);
    let second_candidate = candidate_of(&tree, &second.agent_id);

    tree.adopt_candidate("root", &adopt_req(&first_candidate), None)
        .unwrap();
    let landed = tree
        .adopt_candidate("root", &adopt_req(&second_candidate), None)
        .unwrap_or_else(|error| panic!("the second sibling was refused: {error:?}"));
    assert_eq!(landed.candidate_id, second_candidate);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("OTHER.md")).unwrap(),
        "other\n",
        "the second sibling's delta landed"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("ONE.md")).unwrap(),
        "one\n",
        "the first sibling's delta is still there"
    );
}

/// The git mirror of the same order: two sibling worktrees off one parent,
/// adopted one after the other, both deltas in the parent.
#[test]
fn git_second_sibling_still_adopts_after_the_first_landed() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let second = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let first_path = tree.node(&first.agent_id).unwrap().path.clone();
    let second_path = tree.node(&second.agent_id).unwrap().path.clone();
    std::fs::write(first_path.join("ONE.md"), "one\n").unwrap();
    std::fs::write(second_path.join("OTHER.md"), "other\n").unwrap();
    tree.record_turn_completed(&first.agent_id).unwrap();
    tree.settle_for_test();
    tree.record_turn_completed(&second.agent_id).unwrap();
    tree.settle_for_test();
    let first_candidate = candidate_of(&tree, &first.agent_id);
    let second_candidate = candidate_of(&tree, &second.agent_id);

    tree.adopt_candidate("root", &adopt_req(&first_candidate), None)
        .unwrap();
    tree.adopt_candidate("root", &adopt_req(&second_candidate), None)
        .unwrap_or_else(|error| panic!("the second sibling was refused: {error:?}"));
    assert_eq!(
        std::fs::read_to_string(repo.path().join("OTHER.md")).unwrap(),
        "other\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("ONE.md")).unwrap(),
        "one\n"
    );
}

/// `inputs:[<candidate>]` on jj after that candidate
/// was adopted. The adopt's `jj squash` hides the candidate's change, so a
/// mint that forks by change id alone was refused `mint_unavailable`; the
/// recorded commit id still resolves and is the fallback.
#[test]
fn jj_mint_over_an_already_adopted_candidate_still_forks() {
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
    tree.adopt_candidate("root", &adopt_req(&input), None)
        .unwrap();

    // The reviewer that reads what was just landed.
    let mut over = mint_req();
    over.tier = Tier::Execute;
    over.inputs = Some(vec![input.to_string()]);
    let reviewer = tree.spawn_agent("root", &over, None).unwrap();
    let reviewer_path = tree.node(&reviewer.agent_id).unwrap().path.clone();
    assert_eq!(
        std::fs::read_to_string(reviewer_path.join("FIRST.txt")).unwrap(),
        "first\n",
        "the copy starts from the adopted candidate"
    );
    // An untouched execute copy still seals nothing and discards cleanly.
    tree.record_turn_completed(&reviewer.agent_id).unwrap();
    tree.settle_for_test();
    assert!(
        tree.attempt(&reviewer.agent_id)
            .unwrap()
            .candidate_id
            .is_none()
    );
}

/// The other half of D-J2, and the rule that now stands in front of it: a
/// candidate that a *running* child was minted over is not adoptable —
/// landing it rewrites that child's chain under it. Once the child is
/// finished the landing is allowed, and the child's own candidate must
/// still apply afterwards even though its base was rewritten by that
/// landing (the seal's revset used to fail outright: `Revision ... doesn't
/// exist`, `Change ID ... is divergent`).
#[test]
fn jj_adopt_of_a_base_candidate_waits_for_the_child_holding_it() {
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
    // The child commits, the way a long-running implementer does.
    std::fs::write(second_path.join("SECOND.txt"), "second\n").unwrap();
    jj_commit(&second_path, &["SECOND.txt"], "child commit");
    // Landing its input now would rewrite the copy under it.
    let refused = tree
        .adopt_candidate("root", &adopt_req(&input), None)
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::AdoptNotApplicable);
    assert!(
        refused.message.contains(&second.agent_id)
            && refused.message.contains("wait for it or close it"),
        "{}",
        refused.message
    );
    tree.record_turn_completed(&second.agent_id).unwrap();
    tree.settle_for_test();
    // A sibling copy forked from the parent's own state is not over this
    // candidate and must not block it: the rule names the copies that hold
    // it as their base, not every running child.
    let sibling = tree.spawn_agent("root", &mint_req(), None).unwrap();
    assert_eq!(
        tree.node(&sibling.agent_id).unwrap().status,
        NodeStatus::Running
    );
    // With the child finished, the input lands and rewrites the copy's
    // chain; the candidate the child already sealed must still apply.
    tree.adopt_candidate("root", &adopt_req(&input), None)
        .unwrap();
    assert_ne!(
        tree.node(&second.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired,
        "{:?}",
        tree.fault_reason(&second.agent_id)
    );
    let candidate_id = candidate_of(&tree, &second.agent_id);
    tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("SECOND.txt")).unwrap(),
        "second\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("FIRST.txt")).unwrap(),
        "first\n",
        "the earlier landing is still there"
    );
}

/// The copy stopped being a workspace of the launch
/// repo — `.jj` moved aside and a standalone repo initialized in its place
/// — so every id it reports is foreign. That is a typed seal fault naming
/// the copy, never `candidate: null` and never a candidate built from a
/// repo the parent cannot see.
#[test]
fn jj_copy_that_is_not_a_workspace_faults_the_seal_by_name() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    std::fs::rename(child_path.join(".jj"), child_path.join(".jj.bak")).unwrap();
    jj_in(&child_path, &["git", "init"]);
    assert!(child_path.join(".jj").join("repo").is_dir());

    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    let attempt = tree.attempt(&child.agent_id).unwrap();
    assert!(attempt.candidate_id.is_none(), "nothing foreign was sealed");
    let failure = attempt.failure.as_ref().unwrap();
    assert_eq!(failure.kind, ProductAgentTree::SEAL_FAILED_KIND);
    assert!(
        failure
            .message
            .contains("not a workspace of the launch repo")
            && failure.message.contains("standalone jj repo"),
        "{}",
        failure.message
    );
    assert!(tree.candidates().is_empty());
    // The work is still in the copy; the fault is the account of it.
    assert_eq!(
        std::fs::read_to_string(child_path.join("H.txt")).unwrap(),
        "child\n"
    );
}

/// An `execute` copy on jj that changed nothing seals nothing, so it
/// cannot pin its own workspace against `discard_workspace`. Regression
/// for the live smoke's empty `cand-c2-a1`.
#[test]
fn jj_untouched_execute_copy_seals_nothing_and_discards() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let mut request = mint_req();
    request.tier = Tier::Execute;
    let child = tree.spawn_agent("root", &request, None).unwrap();
    assert_eq!(child.tier, Tier::Execute);
    let base = tree
        .workspace(&child.workspace.id)
        .unwrap()
        .base_commit
        .clone()
        .unwrap_or_else(|| unreachable!("the jj mint records a base"));
    assert_ne!(base, "");
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert!(
        tree.attempt(&child.agent_id)
            .unwrap()
            .candidate_id
            .is_none(),
        "an untouched copy has nothing to keep"
    );
    assert!(tree.candidates().is_empty());
    assert_eq!(
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: child.workspace.id.clone(),
            },
            None,
        )
        .unwrap()
        .status,
        WorkspaceStatus::Discarding
    );
    tree.settle_for_test();
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Discarded
    );
}

/// The same copy that writes a file does seal, so the rule is a real
/// divergence check and not a blanket refusal.
#[test]
fn jj_execute_copy_that_writes_still_seals() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let mut request = mint_req();
    request.tier = Tier::Execute;
    let child = tree.spawn_agent("root", &request, None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    assert!(tree.candidates().contains_key(&candidate_id));
    assert_eq!(
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: child.workspace.id.clone(),
            },
            None,
        )
        .unwrap()
        .status,
        WorkspaceStatus::Discarding
    );
    tree.settle_for_test();
}

/// A jj adopt that conflicts is undone before the squash, so the conflict
/// never lands in the human's `@`.
#[test]
fn jj_adopt_conflict_leaves_the_parent_untouched() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("SEED.txt"), "child side\n").unwrap();
    std::fs::write(repo.path().join("SEED.txt"), "human side\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let error = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap_err();
    assert!(
        matches!(
            error.code,
            TopologyErrorCode::MergeConflict | TopologyErrorCode::TargetChanged
        ),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
        "human side\n",
        "the human's copy is untouched"
    );
    assert!(
        child_path.join("SEED.txt").is_file(),
        "the copy is retained"
    );
}

#[test]
fn repeated_close_during_pending_terminal_seal_keeps_one_ticket_and_close_intent() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(path.join("WORK.txt"), "complete work\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Sealing
    );
    assert_eq!(tree.effects.seals.len(), 1);
    for _ in 0..2 {
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Sealing
        );
        assert_eq!(tree.effects.seals.len(), 1);
    }
    tree.settle_for_test();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert_eq!(
        tree.attempt(&child.agent_id).unwrap().status,
        AttemptStatus::Closed
    );
    assert_eq!(tree.candidates().len(), 1);
    assert_eq!(
        result_mails_from(&tree, &child.agent_id),
        [] as [MailRecord; 0]
    );
    tree.close_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    assert!(tree.effects.seals.is_empty());
}

#[test]
fn close_during_seal_then_retirement_fault_survives_seal_success() {
    close_during_seal_then_retirement_fault(false);
}

#[test]
fn close_during_seal_then_retirement_fault_survives_seal_failure() {
    close_during_seal_then_retirement_fault(true);
}

fn close_during_seal_then_retirement_fault(seal_fails: bool) {
    let (_root, vcs, mut tree) = fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let id = &child.agent_id;
    tree.submit_result(
        id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(id).unwrap();
    let dispatched = tree.take_effects();
    assert_eq!(dispatched.seals.len(), 1);
    tree.close_agent("root", &close_req(id), None).unwrap();
    assert_eq!(tree.node(id).unwrap().status, NodeStatus::Sealing);
    assert_eq!(tree.attempt(id).unwrap().status, AttemptStatus::Closed);
    tree.record_host_retirement(id, "release refused".into())
        .and_then(|value| tree.fold_due(id).map(|_| value))
        .unwrap();
    if seal_fails {
        vcs.seal_next_with(Err(VcsError::seal_failed("snapshot refused")));
    }
    tree.complete(dispatched.run(vcs.as_ref()));
    assert_eq!(tree.node(id).unwrap().status, NodeStatus::RecoveryRequired);
    assert_eq!(tree.attempt(id).unwrap().status, AttemptStatus::Closed);
    assert_eq!(
        tree.attempt(id).unwrap().output_json.as_deref(),
        Some(r#"{"ok":true}"#)
    );
    assert_eq!(result_mails_from(&tree, id), [] as [MailRecord; 0]);
    assert!(matches!(
        tree.recovery_need(id).unwrap(),
        RecoveryNeed::Session { .. }
    ));
    let attempt = tree.attempt(id).unwrap().attempt_id.clone();
    let receipt = tree
        .recover_host_decision(
            id,
            &attempt,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiescent".into(),
            },
        )
        .unwrap();
    assert!(receipt.recovered);
    tree.settle_for_test();
    assert_eq!(tree.node(id).unwrap().status, NodeStatus::Closed);
    assert_eq!(tree.attempt(id).unwrap().status, AttemptStatus::Closed);
    assert_eq!(result_mails_from(&tree, id), [] as [MailRecord; 0]);
}

#[test]
fn retirement_fault_and_repeated_close_keep_dispatched_seal_and_failure() {
    for seal_fails in [false, true] {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        let dispatched = tree.take_effects();
        assert_eq!(dispatched.seals.len(), 1);
        assert!(tree.effects.seals.is_empty());
        tree.record_host_retirement(&child.agent_id, "release refused".into())
            .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
            .unwrap();
        assert!(tree.clone().seal_pending(&child.agent_id));
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        assert!(!reopened.seal_pending(&child.agent_id));
        assert!(
            reopened
                .finish_seal_with(dispatched.seals[0].clone(), Ok(None))
                .is_none()
        );
        assert_eq!(
            reopened
                .attempt(&child.agent_id)
                .unwrap()
                .failure
                .as_ref()
                .unwrap()
                .kind,
            "session_retirement"
        );
        assert_eq!(
            reopened.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        for _ in 0..2 {
            tree.close_agent("root", &close_req(&child.agent_id), None)
                .unwrap();
            tree.stop_all("root", None).unwrap();
            assert!(!tree.begin_seal(&child.agent_id));
            assert!(tree.take_effects().seals.is_empty());
            assert!(tree.seal_pending(&child.agent_id));
            assert_eq!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::RecoveryRequired
            );
        }
        if seal_fails {
            vcs.seal_next_with(Err(VcsError::seal_failed("snapshot refused")));
        }
        tree.complete(dispatched.run(vcs.as_ref()));
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
        assert_eq!(tree.candidates().len(), usize::from(!seal_fails));
        assert!(!tree.seal_pending(&child.agent_id));
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        let attempt = tree.attempt(&child.agent_id).unwrap();
        assert_eq!(attempt.status, AttemptStatus::RecoveryRequired);
        let failure = attempt.failure.as_ref().unwrap();
        assert_eq!(failure.kind, "session_retirement");
        assert!(failure.message.contains("release refused"));
        if seal_fails {
            assert!(failure.message.contains("snapshot refused"));
        }
        assert!(
            tree.fault_reason(&child.agent_id)
                .unwrap()
                .contains("release refused")
        );
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(tree.take_effects().seals.is_empty());
    }
}

#[test]
fn unresolved_terminal_survives_seal_close_adopt_and_bound_replay() {
    use super::super::turn_outcome::FailedTurnDisposition;
    for seal_fails in [false, true] {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        assert!(tree.begin_seal(&child.agent_id));
        let outstanding = tree.take_effects();
        tree.record_turn_failure(
            &child.agent_id,
            FailedTurnDisposition::RecoveryRequired,
            "canonical outcome unknown".into(),
        )
        .unwrap();
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        tree.bind_session_locator(&child.agent_id, "bound-child-session")
            .unwrap();
        tree.record_session_bound_after_failure(&child.agent_id)
            .unwrap();
        tree.fold_due(&child.agent_id).unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(!tree.begin_seal(&child.agent_id));
        if seal_fails {
            vcs.seal_next_with(Err(VcsError::seal_failed("snapshot failed")));
        }
        tree.complete(outstanding.run(vcs.as_ref()));
        if !seal_fails {
            let candidate = candidate_of(&tree, &child.agent_id);
            tree.adopt_candidate("root", &adopt_req(&candidate), None)
                .unwrap();
            assert!(tree.candidates()[&candidate].adopted_by.is_some());
        }
        tree.stop_all("root", None).unwrap();
        tree.record_turn_failure(
            &child.agent_id,
            FailedTurnDisposition::Failed {
                kind: "turn_failed",
            },
            "later terminal failure".into(),
        )
        .unwrap();
        tree.record_host_retirement(&child.agent_id, "release refused".into())
            .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
            .unwrap();
        tree.record_host_retirement(&child.agent_id, "release refused".into())
            .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
            .unwrap();
        let persisted = tree.to_persisted();
        let attempt = persisted.rows().attempt(&child.agent_id).unwrap();
        assert_eq!(attempt.status, AttemptStatus::RecoveryRequired);
        assert_eq!(attempt.output_json.as_deref(), Some(r#"{"ok":true}"#));
        let failure = attempt.failure.as_ref().unwrap();
        for reason in [
            "canonical outcome unknown",
            "later terminal failure",
            "release refused",
        ] {
            assert!(failure.message.contains(reason));
        }
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
        assert!(tree.take_effects().seals.is_empty());
    }
}

#[test]
fn old_seal_completion_cannot_consume_same_attempt_reseal() {
    let (_root, vcs, mut tree) = fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    assert!(tree.begin_seal(&child.agent_id));
    let old = tree.take_effects().seals.pop().unwrap();
    let first = old.seal(vcs.as_ref()).unwrap();
    tree.finish_seal_with(old.clone(), Ok(first.clone()));
    // Force the existing early snapshot to predate the terminal timestamp.
    tree.candidates
        .get_mut(&old.candidate_id)
        .unwrap()
        .sealed_at = 0;
    tree.record_turn_completed(&child.agent_id).unwrap();
    let current = tree.take_effects().seals.pop().unwrap();
    assert_eq!(old.candidate_id, current.candidate_id);
    assert!(tree.finish_seal_with(old, Ok(first)).is_none());
    assert!(tree.seal_pending(&child.agent_id));
    assert!(!tree.begin_seal(&child.agent_id));
    let sealed = current.seal(vcs.as_ref());
    assert!(tree.finish_seal_with(current, sealed).is_some());
    assert!(!tree.seal_pending(&child.agent_id));
    assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 2);
}

/// B1: `close_agent` on a child whose turn is still running does not seal;
/// the candidate is cut when the turn exits, so it holds everything the
/// child wrote up to that point.
#[test]
fn close_while_running_defers_the_seal_to_the_cancelled_turn() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    tree.set_active_turn(&child.agent_id, true).unwrap();
    std::fs::write(child_path.join("BEFORE_CLOSE.txt"), "1\n").unwrap();
    tree.close_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert!(
        tree.candidates().is_empty(),
        "a running child is not sealed at close; its turn is still unwinding"
    );
    assert_eq!(
        result_mails_from(&tree, &child.agent_id),
        [] as [MailRecord; 0]
    );
    // The turn keeps writing until it exits.
    std::fs::write(child_path.join("AFTER_CLOSE.txt"), "2\n").unwrap();
    seal_after_turn_exit(&mut tree, &child.agent_id);
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let content = tree.candidates()[&candidate_id].content.clone();
    let paths = git_tree_paths(repo.path(), &content);
    assert!(paths.iter().any(|p| p == "BEFORE_CLOSE.txt"), "{paths:?}");
    assert!(
        paths.iter().any(|p| p == "AFTER_CLOSE.txt"),
        "the write between close and turn exit is in the candidate: {paths:?}"
    );
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
}

/// B2: an interrupt neither seals nor pushes a `result`; the resumed
/// attempt's completion seals once, with the final content, and pushes
/// exactly one `result`.
#[test]
fn interrupt_seals_nothing_and_completion_pushes_one_result() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    tree.set_active_turn(&child.agent_id, true).unwrap();
    std::fs::write(child_path.join("FIRST.txt"), "1\n").unwrap();
    tree.interrupt_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Interrupted
    );
    // The turn driver's cancelled branch seals only a `Closed` node
    // (`finalize_of_an_interrupted_turn_seals_nothing`), so nothing has
    // sealed and `enqueue_result_mail` refuses the running attempt.
    assert!(tree.candidates().is_empty(), "an interrupt does not seal");
    assert_eq!(
        result_mails_from(&tree, &child.agent_id),
        [] as [MailRecord; 0]
    );
    // Resume: the parent's mail starts a new turn on the same attempt.
    tree.send_agent_message("root", &mail(&child.agent_id, "continue"), None)
        .unwrap();
    tree.set_active_turn(&child.agent_id, true).unwrap();
    std::fs::write(child_path.join("SECOND.txt"), "2\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(tree.candidates().len(), 1);
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let paths = git_tree_paths(repo.path(), &tree.candidates()[&candidate_id].content);
    assert!(paths.iter().any(|p| p == "FIRST.txt"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "SECOND.txt"), "{paths:?}");
    let mails = result_mails_from(&tree, &child.agent_id);
    assert_eq!(mails.len(), 1, "{mails:?}");
    assert!(
        mails[0]
            .body
            .contains(&format!("\"candidate\":\"{candidate_id}\""))
    );
    assert!(!mails[0].body.contains("\"status\":\"running\""));
}

/// B2, second half: a candidate sealed while the attempt was still
/// running is replaced by the seal at termination rather than blocking it.
#[test]
fn a_candidate_sealed_before_the_attempt_ended_is_replaced_at_termination() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("EARLY.txt"), "1\n").unwrap();
    seal_after_turn_exit(&mut tree, &child.agent_id);
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let early = tree.candidates()[&candidate_id].clone();
    assert_eq!(
        tree.attempt(&child.agent_id).unwrap().status,
        AttemptStatus::Running
    );
    std::thread::sleep(std::time::Duration::from_millis(5));
    std::fs::write(child_path.join("LATE.txt"), "2\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(tree.candidates().len(), 1, "same id, replaced in place");
    let final_record = tree.candidates()[&candidate_id].clone();
    assert_ne!(final_record.content, early.content);
    let paths = git_tree_paths(repo.path(), &final_record.content);
    assert!(paths.iter().any(|p| p == "LATE.txt"), "{paths:?}");
    // And a terminal seal is final: a further seal attempt is refused.
    assert!(!tree.begin_seal(&child.agent_id));
}

/// For `close_agent`: an idle closed copy seals as an
/// effect, so the snapshot runs with the tree mutex released.
#[test]
fn close_seal_runs_outside_the_tree_mutex() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.close_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Sealing
    );
    assert!(tree.candidates().is_empty());
    let pending = tree.take_effects();
    assert_eq!(pending.seals.len(), 1);
    tree.complete(pending.run(&SystemVcs));
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert_eq!(tree.candidates().len(), 1);
}

/// the landing subprocess runs on a ticket that holds no tree
/// reference; the row is `Adopting` in between and `Adopted` after.
#[test]
fn adopt_lands_outside_the_tree_mutex() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    tree.begin_adopt("root", &adopt_req(&candidate_id)).unwrap();
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Adopting
    );
    assert!(!repo.path().join("H.txt").exists());
    // The VCS half needs nothing from the tree.
    let pending = tree.take_effects();
    assert_eq!(pending.adopts.len(), 1);
    let mut settled = tree.complete(pending.run(&SystemVcs));
    let outcome = settled.adopts.pop().unwrap().unwrap();
    assert_eq!(outcome.files_changed, 1);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
        "child\n"
    );
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Adopted
    );
    assert_eq!(
        tree.attempt(&child.agent_id).unwrap().status,
        AttemptStatus::Adopted
    );
}

/// A hub with a mutate child it may spawn and adopt from: the hub id, the
/// child id, the child's copy and its sealed candidate.
fn hub_with_sealed_grandchild(
    tree: &mut ProductAgentTree,
) -> (String, String, String, CandidateId) {
    tree.grants.get_mut("root").unwrap().max_depth = 2;
    let mut hub_binding = RosterBinding::leaf("test-account", "selected-tuple");
    hub_binding.spawns = vec![TemplateName::parse("implement").unwrap()];
    hub_binding.may_adopt = true;
    tree.pin_template("orchestrate", hub_binding);
    let hub = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("orchestrate", Tier::Mutate, "hub", "coordinate"),
            None,
        )
        .unwrap();
    tree.grants.get_mut(&hub.agent_id).unwrap().may_spawn = true;
    tree.grants.get_mut(&hub.agent_id).unwrap().may_adopt = true;
    let child = tree.spawn_agent(&hub.agent_id, &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("ONE.txt"), "one\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate = candidate_of(tree, &child.agent_id);
    (hub.agent_id, child.agent_id, child.workspace.id, candidate)
}

/// An adopt records its caller on the row while it lands, and a reopen
/// that finds it in flight faults that adopter, never the copy's owner:
/// the root's adopt of a grandchild's candidate leaves the hub between
/// them alone and becomes the root's fault reason.
#[test]
fn an_interrupted_grandparent_adopt_faults_the_adopter_not_the_hub() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let (hub, _child, workspace, candidate) = hub_with_sealed_grandchild(&mut tree);
    tree.begin_adopt("root", &adopt_req(&candidate)).unwrap();
    let row = tree.workspace(&workspace).unwrap();
    assert_eq!(row.status, WorkspaceStatus::Adopting);
    assert_eq!(
        row.owner_agent_id, hub,
        "the hub owns the copy it spawned into"
    );
    assert_eq!(row.adopter_agent_id.as_deref(), Some("root"));
    let _lost_with_the_process = tree.take_effects();
    tree.restore_paused();
    assert_ne!(
        tree.node(&hub).unwrap().status,
        NodeStatus::RecoveryRequired,
        "the hub's checkout was never written"
    );
    assert_ne!(
        tree.node("root").unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(
        tree.fault_reason("root")
            .is_some_and(|reason| reason.contains("adopt into this checkout"))
    );
}

/// A non-root adopter is faulted along with the copy's occupant; the root
/// is left alone.
#[test]
fn an_interrupted_adopt_faults_a_non_root_adopter() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let (hub, child, workspace, candidate) = hub_with_sealed_grandchild(&mut tree);
    tree.begin_adopt(&hub, &adopt_req(&candidate)).unwrap();
    assert_eq!(
        tree.workspace(&workspace)
            .unwrap()
            .adopter_agent_id
            .as_deref(),
        Some(hub.as_str())
    );
    let _lost_with_the_process = tree.take_effects();
    tree.restore_paused();
    assert_eq!(
        tree.node(&hub).unwrap().status,
        NodeStatus::RecoveryRequired,
        "the adopter's checkout may hold a half-run landing"
    );
    assert_eq!(
        tree.node(&child).unwrap().status,
        NodeStatus::RecoveryRequired,
        "the copy's occupant is faulted as before"
    );
    assert_ne!(
        tree.node("root").unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(tree.fault_reason("root").is_none());
}

#[test]
fn a_live_child_adopt_survives_a_projector_pass_without_a_vcs_fault() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let (hub, child, workspace, candidate) = hub_with_sealed_grandchild(&mut tree);
    tree.begin_adopt(&hub, &adopt_req(&candidate)).unwrap();
    tree.fold_due(&hub).unwrap();
    tree.fold_due(&child).unwrap();
    assert_ne!(
        tree.node(&hub).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_ne!(
        tree.node(&child).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    let pending = tree.take_effects();
    assert!(
        tree.complete(pending.run(&SystemVcs))
            .adopts
            .pop()
            .unwrap()
            .is_ok()
    );
    assert_eq!(
        tree.workspace(&workspace).unwrap().status,
        WorkspaceStatus::Adopted
    );
    assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Adopted);
}

/// The conflict paths read back from the refusal, also when the message
/// bound cut it inside the list.
/// A landing clears the adopter record whichever way it ends.
#[test]
fn a_finished_landing_clears_the_adopter_record() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let (hub, _child, workspace, candidate) = hub_with_sealed_grandchild(&mut tree);
    tree.begin_adopt(&hub, &adopt_req(&candidate)).unwrap();
    let pending = tree.take_effects();
    let mut settled = tree.complete(pending.run(&SystemVcs));
    settled.adopts.pop().unwrap().unwrap();
    let row = tree.workspace(&workspace).unwrap();
    assert_eq!(row.status, WorkspaceStatus::Adopted);
    assert_eq!(row.adopter_agent_id, None);
}

/// B3: a child that commits inside its jj copy and then keeps writing has
/// its whole range landed, not just its working-copy commit.
#[test]
fn jj_committed_work_in_the_child_is_adopted() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("COMMITTED.txt"), "committed\n").unwrap();
    jj_commit(&child_path, &["COMMITTED.txt"], "child commit");
    std::fs::write(child_path.join("WORKING.txt"), "working\n").unwrap();
    // Meanwhile the human keeps editing.
    std::fs::write(repo.path().join("HUMAN.txt"), "human\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let outcome = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap();
    assert_eq!(outcome.files_changed, 2, "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("COMMITTED.txt")).unwrap(),
        "committed\n",
        "the committed change landed"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("WORKING.txt")).unwrap(),
        "working\n",
        "the working-copy change landed"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("HUMAN.txt")).unwrap(),
        "human\n"
    );
}

/// B6: a jj conflict is rolled back with `op restore`, so neither the
/// rebase nor the squash survives in the human's repo.
#[test]
fn jj_conflict_restores_the_operation_log() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("SEED.txt"), "child side\n").unwrap();
    jj_commit(&child_path, &[], "child edit");
    std::fs::write(child_path.join("MORE.txt"), "more\n").unwrap();
    std::fs::write(repo.path().join("SEED.txt"), "human side\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let error = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::MergeConflict, "{error:?}");
    assert!(error.message.contains("SEED.txt"), "{}", error.message);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
        "human side\n"
    );
    assert!(
        !repo.path().join("MORE.txt").exists(),
        "nothing was squashed"
    );
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Active,
        "the copy is retained"
    );
}

/// the copy's base advances to what was adopted, so once a
/// hub has landed one of a child's candidates, a *different* node landing
/// a later one would receive only the second delta. Refused, and the
/// refusal names the node that holds the first.
#[test]
fn a_second_adopter_of_the_same_copy_is_refused_and_names_the_first() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    tree.grants.get_mut("root").unwrap().max_depth = 2;
    let mut hub_binding = RosterBinding::leaf("test-account", "selected-tuple");
    hub_binding.spawns = vec![TemplateName::parse("implement").unwrap()];
    hub_binding.may_adopt = true;
    tree.pin_template("orchestrate", hub_binding);
    let mut request =
        SpawnAgentRequest::test_named("orchestrate", Tier::Mutate, "hub", "coordinate");
    request.workspace = SpawnWorkspace::Fork;
    let hub = tree.spawn_agent("root", &request, None).unwrap();
    tree.grants.get_mut(&hub.agent_id).unwrap().may_spawn = true;
    tree.grants.get_mut(&hub.agent_id).unwrap().may_adopt = true;
    let child = tree.spawn_agent(&hub.agent_id, &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("ONE.txt"), "one\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let first = candidate_of(&tree, &child.agent_id);
    tree.adopt_candidate(&hub.agent_id, &adopt_req(&first), None)
        .unwrap();

    let mut followup = mail(&child.agent_id, "one more");
    followup.purpose = Some(MessagePurpose::Followup);
    tree.send_agent_message(&hub.agent_id, &followup, None)
        .unwrap();
    std::fs::write(child_path.join("TWO.txt"), "two\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let second = candidate_of(&tree, &child.agent_id);
    let error = tree
        .adopt_candidate("root", &adopt_req(&second), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
    assert!(error.message.contains(&hub.agent_id), "{}", error.message);
    assert!(error.message.contains(first.as_str()), "{}", error.message);
    assert!(
        !error.message.contains("  "),
        "the refusal must not run spaces together: {}",
        error.message
    );
    // The hub itself may still land its own next candidate.
    assert_eq!(
        tree.adopt_candidate(&hub.agent_id, &adopt_req(&second), None)
            .unwrap()
            .candidate_id,
        second
    );
}

/// Adopting `a1` while `a2` runs is refused — but "the producing attempt
/// has not finished" described an attempt that had in fact finished. The
/// refusal now names the node and the attempt it is talking about.
#[test]
fn adopting_an_earlier_attempt_while_a_later_one_runs_names_the_node() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("ONE.txt"), "one\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let first = candidate_of(&tree, &child.agent_id);
    let mut followup = mail(&child.agent_id, "keep going");
    followup.purpose = Some(MessagePurpose::Followup);
    tree.send_agent_message("root", &followup, None).unwrap();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Running
    );
    let error = tree
        .adopt_candidate("root", &adopt_req(&first), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::NotSealed);
    assert!(
        error.message.contains(&child.agent_id)
            && error.message.contains("later attempt")
            && !error
                .message
                .contains("the producing attempt has not finished")
            && !error.message.contains("  "),
        "{}",
        error.message
    );
}

/// adoption authority is subtree membership. A sibling is not
/// in the caller's subtree however wide its own grant is.
#[test]
fn adopting_outside_the_callers_subtree_is_refused() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let producer = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let producer_path = tree.node(&producer.agent_id).unwrap().path.clone();
    std::fs::write(producer_path.join("H.txt"), "child\n").unwrap();
    tree.record_turn_completed(&producer.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &producer.agent_id);
    let sibling = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.grants.get_mut(&sibling.agent_id).unwrap().may_adopt = true;
    let error = tree
        .adopt_candidate(&sibling.agent_id, &adopt_req(&candidate_id), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::NotAuthorized);
    assert!(error.message.contains("subtree"), "{}", error.message);
    assert!(
        tree.candidates()
            .get(&candidate_id)
            .unwrap()
            .adopted_by
            .is_none(),
        "nothing landed"
    );
}

/// An execute copy that never diverged pinned
/// `refs/plak/<namespace>/candidates/<id>` while the tree filtered the candidate out,
/// so "a candidate ref is pinned" could pass on a phantom. No candidate,
/// no ref.
#[test]
fn a_seal_with_no_divergence_leaves_no_candidate_ref() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert!(tree.candidates().is_empty());
    let expected_ref = plakous_workspace::occupancy_vcs::candidate_ref(
        &digest_text(&tree.tree_id),
        &format!("cand-{}", child.attempt_id),
    );
    let refs = Command::new("git")
        .args(["for-each-ref", "refs/plak/"])
        .current_dir(&child_path)
        .output()
        .unwrap();
    let refs = String::from_utf8_lossy(&refs.stdout);
    assert!(!refs.contains(&expected_ref), "orphan pin survived: {refs}");
    // The diverging case still pins.
    let mut followup = mail(&child.agent_id, "write something");
    followup.purpose = Some(MessagePurpose::Followup);
    tree.send_agent_message("root", &followup, None).unwrap();
    std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let refs = Command::new("git")
        .args(["for-each-ref", "refs/plak/"])
        .current_dir(&child_path)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&refs.stdout).contains(
        &plakous_workspace::occupancy_vcs::candidate_ref(
            &digest_text(&tree.tree_id),
            candidate_id.as_str()
        )
    ));
}

/// Regression: adopt -> followup -> adopt. The
/// adopted copy hosts a second attempt; its candidate is the delta since
/// the adoption, and the second landing integrates only that delta — both
/// lines land once, uncommitted, HEAD unchanged.
#[test]
fn followup_after_adoption_seals_and_adopts_only_the_delta() {
    let repo = git_repo();
    let head = git_head(repo.path());
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("NOTES.md"), "delegated\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let first = candidate_of(&tree, &child.agent_id);
    let landed = tree
        .adopt_candidate("root", &adopt_req(&first), None)
        .unwrap();
    assert_eq!(landed.files_changed, 1);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("NOTES.md")).unwrap(),
        "delegated\n"
    );
    let row = tree.workspace(&child.workspace.id).unwrap().clone();
    assert_eq!(row.status, WorkspaceStatus::Adopted);
    let adopted = tree.candidates().get(&first).unwrap().clone();
    assert_eq!(
        row.base_commit.as_deref(),
        Some(adopted.commit.as_str()),
        "the copy's base advanced to the adopted candidate"
    );
    assert_eq!(row.base_content.as_deref(), Some(adopted.content.as_str()));

    let mut followup = mail(&child.agent_id, "add a second line");
    followup.purpose = Some(MessagePurpose::Followup);
    let sent = tree.send_agent_message("root", &followup, None).unwrap();
    assert_eq!(sent.disposition, MailDisposition::Started);
    assert_eq!(
        tree.workspace(&child.workspace.id).unwrap().status,
        WorkspaceStatus::Active,
        "the adopted copy is live again for the new attempt"
    );
    let second_attempt = tree.attempt(&child.agent_id).unwrap().attempt_id.clone();
    assert_ne!(second_attempt, child.attempt_id);
    std::fs::write(child_path.join("NOTES.md"), "delegated\nsecond\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let second = candidate_of(&tree, &child.agent_id);
    assert_ne!(second, first);
    let record = tree.candidates().get(&second).unwrap();
    assert_eq!(
        record.base_commit, adopted.commit,
        "the second candidate's merge base is the adopted state"
    );

    let landed = tree
        .adopt_candidate("root", &adopt_req(&second), None)
        .unwrap();
    assert_eq!(landed.files_changed, 1, "{landed:?}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("NOTES.md")).unwrap(),
        "delegated\nsecond\n",
        "both lines, each once"
    );
    assert_eq!(git_head(repo.path()), head, "no commit in the root");
    assert!(
        git_porcelain(repo.path()).contains("NOTES.md"),
        "the landing is uncommitted"
    );
    assert_eq!(
        tree.attempts.get(&child.attempt_id).unwrap().status,
        AttemptStatus::Adopted
    );
    assert_eq!(
        tree.attempt(&child.agent_id).unwrap().status,
        AttemptStatus::Adopted
    );
}

/// The jj shape of adopt -> followup -> adopt: the squash rewrote the
/// child's `@`, so the copy's base is its post-landing working copy and a
/// followup that changes nothing seals no candidate.
#[test]
fn jj_followup_after_adoption_adopts_only_the_delta() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("NOTES.md"), "delegated\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let first = candidate_of(&tree, &child.agent_id);
    tree.adopt_candidate("root", &adopt_req(&first), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("NOTES.md")).unwrap(),
        "delegated\n"
    );
    let mut followup = mail(&child.agent_id, "add a second line");
    followup.purpose = Some(MessagePurpose::Followup);
    tree.send_agent_message("root", &followup, None).unwrap();
    // Nothing changed in the copy: no candidate, no phantom divergence
    // from the rewritten `@`.
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(tree.candidates().len(), 1, "{:?}", tree.candidates());

    let mut again = mail(&child.agent_id, "really add it");
    again.purpose = Some(MessagePurpose::Followup);
    tree.send_agent_message("root", &again, None).unwrap();
    std::fs::write(child_path.join("NOTES.md"), "delegated\nsecond\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let second = candidate_of(&tree, &child.agent_id);
    assert_ne!(second, first);
    let landed = tree
        .adopt_candidate("root", &adopt_req(&second), None)
        .unwrap();
    assert_eq!(landed.files_changed, 1, "{landed:?}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("NOTES.md")).unwrap(),
        "delegated\nsecond\n"
    );
}

/// `require_review_evidence`: when the epoch demands it, an
/// adopt must cite a subtree attempt whose output names the candidate.
#[test]
fn require_review_evidence_demands_a_review_that_names_the_candidate() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    tree.snapshot.require_review_evidence = true;
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("H.txt"),
        "child\n",
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate_id = candidate_of(&tree, &child.agent_id);
    let error = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::EvidenceMismatch);
    assert!(
        error.message.contains(candidate_id.as_str()),
        "{}",
        error.message
    );
    // A reviewer whose output does not mention the candidate is not
    // evidence about it.
    let mut review = spawn_req();
    review.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"ok":{"type":"boolean"},"reviewed":{"type":"string"}},"required":["ok"],"additionalProperties":false}"#.into();
    let reviewer = tree.spawn_agent("root", &review, None).unwrap();
    tree.submit_result(
        &reviewer.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true,"reviewed":"nothing in particular"}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&reviewer.agent_id).unwrap();
    tree.settle_for_test();
    let mut with_evidence = adopt_req(&candidate_id);
    with_evidence.evidence = Some(vec![reviewer.attempt_id.to_string()]);
    assert_eq!(
        tree.adopt_candidate("root", &with_evidence, None)
            .unwrap_err()
            .code,
        TopologyErrorCode::EvidenceMismatch
    );
    // One that does is.
    let second = tree.spawn_agent("root", &review, None).unwrap();
    tree.submit_result(
        &second.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: format!(r#"{{"ok":true,"reviewed":"{candidate_id}"}}"#),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&second.agent_id).unwrap();
    tree.settle_for_test();
    with_evidence.evidence = Some(vec![second.attempt_id.to_string()]);
    let outcome = tree.adopt_candidate("root", &with_evidence, None).unwrap();
    assert_eq!(outcome.candidate_id, candidate_id);
    assert_eq!(
        tree.candidates()[&candidate_id].adopted_evidence,
        vec![second.attempt_id.to_string()]
    );
}
