#![allow(clippy::unwrap_used, reason = "real Git resolver retention probes")]

use crate::protocol::{CandidateId, NodeStatus, WriterMode};
use crate::tree::test_support::{adopt_req, candidate_of, close_req, mint_req, tree_at};
use crate::tree::{AwaitAgents, ListAgentsRequest, ProductAgentTree};

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
fn a_missing_input_record_fails_sealing_closed_instead_of_erasing_lineage() {
    let mut f = Fixture::new();
    let resolver = f.resolver();
    f.tree.candidates.remove(&f.conflict);
    f.tree.record_turn_completed(&resolver).unwrap();
    f.tree.settle_for_test();
    assert!(f.tree.attempt(&resolver).unwrap().candidate_id.is_none());
    assert_eq!(
        f.tree.node(&resolver).unwrap().status,
        NodeStatus::RecoveryRequired
    );
}
