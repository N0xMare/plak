#![allow(
    clippy::too_many_lines,
    reason = "conflict scenarios verify source, resolver and accepted-target provenance across the entire landing"
)]
#![allow(clippy::unwrap_used, reason = "real Git conflict collection fixtures")]

use crate::protocol::WriterMode;
use crate::tree::test_support::{adopt_req, candidate_of, close_req, mint_req, tree_at};
use crate::tree::{AwaitAgents, CandidateKind, ListAgentsRequest, ProductAgentTree};
use plakous_workspace::occupancy_vcs::{SystemVcs, VcsOps};

fn conflict(
    tree: &mut ProductAgentTree,
    source: &crate::protocol::CandidateId,
) -> crate::protocol::CandidateId {
    assert!(
        tree.adopt_candidate("root", &adopt_req(source), None)
            .is_err()
    );
    tree.candidates
        .values()
        .filter(|row| row.kind == CandidateKind::Conflict)
        .max_by_key(|row| row.candidate_id.clone())
        .unwrap()
        .candidate_id
        .clone()
}

#[test]
fn direct_source_acceptance_collects_stale_conflict_candidates_and_unpins_refs() {
    collection_law(false);
}

#[test]
fn resolution_collects_the_losing_resolver_and_unpins_refs() {
    collection_law(true);
}

fn collection_law(resolve: bool) {
    let repo = plakous_workspace::vcs_fixtures::git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(path.join("SEED.txt"), "child\n").unwrap();
    std::fs::write(repo.path().join("SEED.txt"), "parent\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let source = candidate_of(&tree, &child.agent_id);
    let x1 = conflict(&mut tree, &source);
    let x2 = conflict(&mut tree, &source);
    assert_ne!(x1, x2);
    let mut spent = vec![x1.clone(), x2];
    if resolve {
        let mut resolvers = Vec::new();
        for _ in 0..2 {
            let mut request = mint_req();
            request.inputs = Some(vec![x1.to_string()]);
            let resolver = tree.spawn_agent("root", &request, None).unwrap();
            let path = tree.node(&resolver.agent_id).unwrap().path.clone();
            std::fs::write(path.join("SEED.txt"), "resolved\n").unwrap();
            tree.record_turn_completed(&resolver.agent_id).unwrap();
            tree.settle_for_test();
            resolvers.push(candidate_of(&tree, &resolver.agent_id));
        }
        tree.adopt_candidate("root", &adopt_req(&resolvers[0]), None)
            .unwrap();
        spent.push(resolvers[1].clone());
    } else {
        std::fs::write(repo.path().join("SEED.txt"), "seed\n").unwrap();
        tree.adopt_candidate("root", &adopt_req(&source), None)
            .unwrap();
    }
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_completed: Some(true),
                include_self: None,
                include_read: None,
            },
        )
        .unwrap();
    for id in &spent {
        let row = &tree.candidates[id];
        if !resolve || id != &x1 {
            assert!(row.adopted_by.is_none());
        }
        assert!(row.adopted_via.is_some());
        assert!(
            !listed
                .agents
                .iter()
                .any(|row| row["candidates"]
                    .as_array()
                    .is_some_and(|candidates| candidates
                        .iter()
                        .any(|candidate| candidate["candidate_id"] == id.as_str())))
        );
    }
    let wait = tree
        .poll_wait("root", &AwaitAgents::Pending)
        .unwrap()
        .unwrap();
    tree.acknowledge_wait_mail("root", &wait);
    let children: Vec<_> = tree
        .nodes
        .keys()
        .filter(|id| id.as_str() != "root")
        .cloned()
        .collect();
    for agent in children {
        tree.close_agent("root", &close_req(&agent), None).unwrap();
    }
    tree.settle_for_test();
    tree.reclaim_dormant(u64::MAX, &std::collections::BTreeMap::new())
        .unwrap();
    tree.settle_for_test();
    for ticket in tree.prepare_child_retention(u64::MAX).unwrap() {
        tree.finish_child_retention(&ticket).unwrap();
    }
    for id in &spent {
        assert!(!tree.candidates.contains_key(id));
    }
    for cleanup in tree.pending_pin_cleanup().unwrap() {
        for id in &cleanup.candidates {
            crate::tree::block_on_for_test(SystemVcs.unpin(
                repo.path(),
                &super::super::digest_text("tree1"),
                id.as_str(),
            ))
            .unwrap();
        }
        tree.acknowledge_pin_cleanup(&cleanup).unwrap();
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["for-each-ref", "--format=%(refname)", "refs/plak/"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let refs = String::from_utf8(out.stdout).unwrap();
    for id in spent {
        assert!(!refs.contains(id.as_str()), "{refs}");
    }
}
