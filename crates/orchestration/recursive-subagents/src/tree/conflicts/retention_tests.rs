#![allow(clippy::unwrap_used, reason = "real Git resolver retention probes")]

use crate::protocol::{CandidateId, NodeStatus, WriterMode};
use crate::tree::test_support::{adopt_req, candidate_of, close_req, mint_req, tree_at};
use crate::tree::{AwaitAgents, ListAgentsRequest, ProductAgentTree};

fn acknowledge(tree: &mut ProductAgentTree, agent: &str) {
    let mail = tree
        .timeout_wait_rows_for_test("root", &AwaitAgents::Named(vec![agent.to_owned()]))
        .unwrap();
    for row in mail.mail {
        tree.acknowledge_mail(&row.message_id).unwrap();
    }
}

struct Fixture {
    repo: tempfile::TempDir,
    tree: ProductAgentTree,
    source: CandidateId,
    conflict: CandidateId,
}

impl Fixture {
    fn new() -> Self {
        let repo = plakous_workspace::vcs_fixtures::git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(path.join("SEED.txt"), "child\n").unwrap();
        std::fs::write(repo.path().join("SEED.txt"), "parent\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let source = candidate_of(&tree, &child.agent_id);
        assert!(
            tree.adopt_candidate("root", &adopt_req(&source), None)
                .is_err()
        );
        let conflict = tree
            .candidates
            .values()
            .find(|c| c.source_candidate_id.as_ref() == Some(&source))
            .unwrap()
            .candidate_id
            .clone();
        Self {
            repo,
            tree,
            source,
            conflict,
        }
    }

    fn resolver(&mut self) -> String {
        let mut request = mint_req();
        request.inputs = Some(vec![self.conflict.to_string()]);
        self.tree
            .spawn_agent("root", &request, None)
            .unwrap()
            .agent_id
    }

    fn finish(&mut self, agent: &str, resolved: bool) -> CandidateId {
        let path = self.tree.node(agent).unwrap().path.clone();
        if resolved {
            std::fs::write(path.join("SEED.txt"), "parent\n").unwrap();
        }
        std::fs::write(path.join(format!("{agent}.txt")), "resolver work\n").unwrap();
        self.tree.record_turn_completed(agent).unwrap();
        self.tree.settle_for_test();
        candidate_of(&self.tree, agent)
    }

    fn listed(&self, id: &CandidateId) -> bool {
        self.tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_completed: Some(true),
                    include_self: None,
                    include_read: None,
                },
            )
            .unwrap()
            .agents
            .iter()
            .any(|row| {
                row["candidates"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|row| row["candidate_id"] == id.as_str()))
            })
    }
}

#[test]
fn collected_source_and_winning_resolver_cannot_erase_a_live_resolvers_marker_fence() {
    let mut f = Fixture::new();
    let winner = f.resolver();
    let loser = f.resolver();
    let candidate = f.finish(&winner, true);
    f.tree
        .adopt_candidate("root", &adopt_req(&candidate), None)
        .unwrap();
    let original = std::fs::read(f.repo.path().join("SEED.txt")).unwrap();
    assert_eq!(original, b"parent\n");
    let source_agent = f.tree.candidates[&f.source].producing_agent_id.clone();
    for agent in [&source_agent, &winner] {
        let mail = f
            .tree
            .timeout_wait_rows_for_test("root", &AwaitAgents::Named(vec![agent.clone()]))
            .unwrap();
        for row in mail.mail {
            f.tree.acknowledge_mail(&row.message_id).unwrap();
        }
        f.tree.close_agent("root", &close_req(agent), None).unwrap();
    }
    f.tree.settle_for_test();
    f.tree
        .reclaim_dormant(u64::MAX, &std::collections::BTreeMap::new())
        .unwrap();
    f.tree.settle_for_test();
    for ticket in f.tree.prepare_child_retention(u64::MAX).unwrap() {
        f.tree.finish_child_retention(&ticket).unwrap();
    }
    assert!(f.tree.conversation_collected(&source_agent));
    assert!(!f.tree.candidates.contains_key(&candidate));
    assert!(f.tree.candidates.contains_key(&f.conflict));
    for cleanup in f.tree.pending_pin_cleanup().unwrap() {
        assert!(!cleanup.candidates.contains(&f.conflict));
    }
    let namespace = &super::super::digest_text("tree1")[..16];
    let reference = format!("refs/plak/{namespace}/candidates/{}", f.conflict);
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(f.repo.path())
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .unwrap()
            .success()
    );
    let late = f.finish(&loser, false);
    let row = &f.tree.candidates[&late];
    assert_eq!(row.resolves.as_ref(), Some(&f.source));
    assert_eq!(row.adopted_via.as_ref(), Some(&candidate));
    assert!(!f.listed(&late));
    let error = f
        .tree
        .adopt_candidate("root", &adopt_req(&late), None)
        .unwrap_err();
    assert_eq!(
        error.code,
        crate::protocol::TopologyErrorCode::MergeConflict
    );
    assert!(error.message.contains("unresolved markers"));
    assert_eq!(
        std::fs::read(f.repo.path().join("SEED.txt")).unwrap(),
        original
    );
}

#[test]
fn a_late_resolver_seal_is_superseded_and_a_spent_conflict_cannot_mint_another() {
    let mut f = Fixture::new();
    let resolver = f.resolver();
    std::fs::write(f.repo.path().join("SEED.txt"), "seed\n").unwrap();
    f.tree
        .adopt_candidate("root", &adopt_req(&f.source), None)
        .unwrap();
    let late = f.finish(&resolver, true);
    assert_eq!(
        f.tree.candidates[&late].adopted_via.as_ref(),
        Some(&f.source)
    );
    assert!(!f.listed(&late));
    let mut request = mint_req();
    request.inputs = Some(vec![f.conflict.to_string()]);
    let error = f.tree.spawn_agent("root", &request, None).unwrap_err();
    assert_eq!(
        error.code,
        crate::protocol::TopologyErrorCode::AdoptNotApplicable
    );
    assert!(
        error
            .message
            .contains(&format!("resolution already accepted via {}", f.source))
    );
}

#[test]
fn inconsistent_persisted_resolver_lineage_fails_sealing_closed() {
    let mut f = Fixture::new();
    let resolver = f.resolver();
    let workspace = f.tree.node(&resolver).unwrap().workspace_id.clone();
    f.tree.workspaces.get_mut(&workspace).unwrap().conflict_base = None;
    f.tree.record_turn_completed(&resolver).unwrap();
    f.tree.settle_for_test();
    assert!(f.tree.attempt(&resolver).unwrap().candidate_id.is_none());
    assert_eq!(
        f.tree.node(&resolver).unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

#[test]
fn ordinary_stacked_child_over_collected_input_seals_at_turn_end_and_dormancy() {
    for dormant in [false, true] {
        let repo = plakous_workspace::vcs_fixtures::git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&first.agent_id).unwrap().path.clone();
        std::fs::write(path.join("FIRST.txt"), "first\n").unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        let input = candidate_of(&tree, &first.agent_id);
        tree.adopt_candidate("root", &adopt_req(&input), None)
            .unwrap();
        let mut request = mint_req();
        request.inputs = Some(vec![input.to_string()]);
        let second = tree.spawn_agent("root", &request, None).unwrap();
        acknowledge(&mut tree, &first.agent_id);
        tree.close_agent("root", &close_req(&first.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        tree.reclaim_dormant(u64::MAX, &std::collections::BTreeMap::new())
            .unwrap();
        tree.settle_for_test();
        for ticket in tree.prepare_child_retention(u64::MAX).unwrap() {
            tree.finish_child_retention(&ticket).unwrap();
        }
        assert!(!tree.candidates.contains_key(&input));
        let path = tree.node(&second.agent_id).unwrap().path.clone();
        if dormant {
            tree.record_turn_completed(&second.agent_id).unwrap();
            tree.settle_for_test();
            acknowledge(&mut tree, &second.agent_id);
        }
        std::fs::write(path.join("SECOND.txt"), "second\n").unwrap();
        if dormant {
            tree.reclaim_dormant(u64::MAX, &std::collections::BTreeMap::new())
                .unwrap();
        } else {
            tree.record_turn_completed(&second.agent_id).unwrap();
        }
        let settled = tree.settle_for_test();
        assert_eq!(settled.seal_failures, Vec::new());
        assert_eq!(settled.cleanup_failures, Vec::new());
        let candidate = tree
            .candidates
            .values()
            .find(|row| row.producing_agent_id == second.agent_id)
            .unwrap();
        assert!(candidate.resolves.is_none());
        let id = candidate.candidate_id.clone();
        tree.adopt_candidate("root", &adopt_req(&id), None).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("SECOND.txt")).unwrap(),
            "second\n"
        );
    }
}

#[test]
fn accepted_resolver_rehydrates_after_its_source_and_conflict_are_collected_then_seals_work() {
    use crate::protocol::{MessagePurpose, SendAgentMessageRequest};
    let mut f = Fixture::new();
    let resolver = f.resolver();
    let accepted = f.finish(&resolver, true);
    f.tree
        .adopt_candidate("root", &adopt_req(&accepted), None)
        .unwrap();
    let source_agent = f.tree.candidates[&f.source].producing_agent_id.clone();
    acknowledge(&mut f.tree, &source_agent);
    acknowledge(&mut f.tree, &resolver);
    f.tree
        .close_agent("root", &close_req(&source_agent), None)
        .unwrap();
    f.tree.settle_for_test();
    f.tree
        .reclaim_dormant(u64::MAX, &std::collections::BTreeMap::new())
        .unwrap();
    f.tree.settle_for_test();
    let workspace = f.tree.node(&resolver).unwrap().workspace_id.clone();
    assert!(f.tree.reclaimed(&workspace));
    for ticket in f
        .tree
        .prepare_child_retention(super::super::now_unix_millis())
        .unwrap()
    {
        assert_eq!(ticket.agent_id, source_agent);
        f.tree.finish_child_retention(&ticket).unwrap();
    }
    assert!(!f.tree.candidates.contains_key(&f.source));
    assert!(!f.tree.candidates.contains_key(&f.conflict));
    let request = SendAgentMessageRequest {
        agent_id: resolver.clone(),
        message: "follow up".into(),
        purpose: Some(MessagePurpose::Followup),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    };
    f.tree
        .rehydrate_followup("root", &request, "revive")
        .unwrap();
    f.tree.settle_for_test();
    f.tree
        .send_agent_message("root", &request, Some("revive"))
        .unwrap();
    let path = f.tree.node(&resolver).unwrap().path.clone();
    std::fs::write(path.join("FOLLOWUP.txt"), "ordinary delta\n").unwrap();
    f.tree.record_turn_completed(&resolver).unwrap();
    f.tree.settle_for_test();
    let next = candidate_of(&f.tree, &resolver);
    let row = &f.tree.candidates[&next];
    assert!(row.resolves.is_none());
    assert_eq!(row.conflict_paths, Vec::new());
    f.tree
        .adopt_candidate("root", &adopt_req(&next), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("FOLLOWUP.txt")).unwrap(),
        "ordinary delta\n"
    );
}

#[test]
fn superseded_resolutions_explain_that_the_accepting_candidate_was_reverted() {
    let mut f = Fixture::new();
    let first = f.resolver();
    let second = f.resolver();
    let accepted = f.finish(&first, true);
    let losing = f.finish(&second, true);
    assert!(
        f.tree
            .adopt_candidate("root", &adopt_req(&f.source), None)
            .is_err()
    );
    let other = f
        .tree
        .candidates
        .values()
        .find(|row| {
            row.source_candidate_id.as_ref() == Some(&f.source) && row.candidate_id != f.conflict
        })
        .unwrap()
        .candidate_id
        .clone();
    f.tree
        .adopt_candidate("root", &adopt_req(&accepted), None)
        .unwrap();
    let mut reverse = adopt_req(&accepted);
    reverse.revert = true;
    f.tree.adopt_candidate("root", &reverse, None).unwrap();
    let expected = format!(
        "superseded by {accepted}, since reverted; adopt {} again or let a new conflict pin a fresh candidate",
        f.source
    );
    let error = f
        .tree
        .adopt_candidate("root", &adopt_req(&losing), None)
        .unwrap_err();
    assert_eq!(
        error.code,
        crate::protocol::TopologyErrorCode::AdoptNotApplicable
    );
    assert_eq!(error.message, expected);
    let mut spawn = mint_req();
    spawn.inputs = Some(vec![other.to_string()]);
    assert_eq!(
        f.tree
            .spawn_agent("root", &spawn, None)
            .unwrap_err()
            .message,
        expected
    );
    assert_eq!(
        f.tree.candidates[&losing].adopted_via.as_ref(),
        Some(&accepted)
    );
    f.tree
        .adopt_candidate("root", &adopt_req(&accepted), None)
        .unwrap();
    for id in [&accepted, &f.source, &f.conflict] {
        assert!(f.tree.candidates[id].reverted_at.is_none());
        assert!(f.tree.candidates[id].reverted_by.is_none());
    }
}

#[test]
fn reverted_resolver_readopts_after_its_source_is_collected_and_its_copy_reclaimed() {
    let mut f = Fixture::new();
    let resolver = f.resolver();
    let accepted = f.finish(&resolver, true);
    f.tree
        .adopt_candidate("root", &adopt_req(&accepted), None)
        .unwrap();
    let source_agent = f.tree.candidates[&f.source].producing_agent_id.clone();
    acknowledge(&mut f.tree, &source_agent);
    acknowledge(&mut f.tree, &resolver);
    f.tree
        .close_agent("root", &close_req(&source_agent), None)
        .unwrap();
    f.tree.settle_for_test();
    f.tree
        .reclaim_dormant(u64::MAX, &std::collections::BTreeMap::new())
        .unwrap();
    f.tree.settle_for_test();
    for ticket in f
        .tree
        .prepare_child_retention(super::super::now_unix_millis())
        .unwrap()
    {
        assert_eq!(ticket.agent_id, source_agent);
        f.tree.finish_child_retention(&ticket).unwrap();
    }
    assert!(!f.tree.candidates.contains_key(&f.source));
    let mut reverse = adopt_req(&accepted);
    reverse.revert = true;
    f.tree.adopt_candidate("root", &reverse, None).unwrap();
    assert!(!f.repo.path().join(format!("{resolver}.txt")).exists());
    f.tree
        .adopt_candidate("root", &adopt_req(&accepted), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join(format!("{resolver}.txt"))).unwrap(),
        "resolver work\n"
    );
}
