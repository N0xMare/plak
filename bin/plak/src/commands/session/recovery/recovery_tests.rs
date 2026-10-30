use super::*;
use plakous_hosted::topology_runtime::test_support::bootstrap;

#[test]
fn holder_status_probe_is_read_only_and_distinguishes_locked_from_abandoned() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("holder.lock");
    assert!(!holder_is_live(&path).unwrap());
    assert!(!path.exists());
    let holder = std::fs::File::create(&path).unwrap();
    assert!(!holder_is_live(&path).unwrap());
    rustix::fs::flock(&holder, rustix::fs::FlockOperation::LockExclusive).unwrap();
    assert!(holder_is_live(&path).unwrap());
    drop(holder);
    assert!(!holder_is_live(&path).unwrap());
    std::fs::remove_file(&path).unwrap();
    assert!(!holder_is_live(&path).unwrap());
    assert!(holder_is_live(&home.path().join("missing-parent/holder.lock")).is_ok());
    assert!(!home.path().join("missing-parent").exists());
}

#[test]
fn session_show_reports_candidate_merged_at() {
    use plakous_hosted::agent_tree_protocol::{AdoptCandidateRequest, SpawnAgentRequest};
    let (_home, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    paths.ensure_layout().unwrap();
    let record = product_sessions::SessionRecord::new(repo.path().to_path_buf());
    let mut tree = bootstrap(&record.id, repo.path().to_path_buf());
    let child = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named(
                "implement",
                plakous_hosted::agent_tree_protocol::Tier::Mutate,
                "editor",
                "implement",
            )
            .with_fork(),
            None,
        )
        .unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("merged.txt"),
        "merged content",
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let id = tree
        .candidates()
        .values()
        .next()
        .unwrap()
        .candidate_id
        .clone();
    let adopted = tree
        .adopt_candidate(
            "root",
            &AdoptCandidateRequest {
                revert: false,
                accept_conflicts: false,
                candidate_id: id.clone(),
                expected_target: None,
                evidence: None,
            },
            None,
        )
        .unwrap();
    tree.bind_session_locator(&child.agent_id,
        "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003"
    ).unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let mut output = Vec::new();
    show_child_recovery_status_with(&paths, &record, &mut output, |locator| Ok((locator, None)))
        .unwrap();
    assert!(String::from_utf8(output).unwrap().contains(&format!(
        "Candidate      {id} merged at {}",
        adopted.target_after
    )));
}

#[test]
fn child_inspection_continues_past_bad_first_locator_and_returns_incomplete() {
    let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let record = product_sessions::SessionRecord::new(paths.home().to_path_buf());
    let mut tree = bootstrap(&record.id, paths.home().to_path_buf());
    let first = tree
        .spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
    let second = tree
        .spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
    tree.bind_session_locator(&first.agent_id, "invalid locator")
        .unwrap();
    let valid = "00000000-0000-0000-0000-000000000001/\
                     00000000-0000-0000-0000-000000000002/\
                     00000000-0000-0000-0000-000000000003";
    tree.bind_session_locator(&second.agent_id, valid).unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();

    let mut output = Vec::new();
    let mut calls = Vec::new();
    let result = show_child_recovery_status_with(&paths, &record, &mut output, |locator| {
        calls.push(locator);
        Ok((locator, None))
    });
    let output = String::from_utf8(output).unwrap();
    assert!(result.unwrap_err().is_recovery_required());
    assert_eq!(calls.len(), 1, "only the later valid locator is inspected");
    assert_eq!(calls[0].to_string(), valid);
    assert!(output.contains("invalid recorded locator"));
    assert!(output.contains(&format!("Child          {}", second.agent_id)));
    assert!(output.contains(&format!("quiescent at {valid}")));
    assert!(output.find("invalid recorded locator") < output.find("quiescent at"));
}
