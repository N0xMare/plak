//! Real Git laws for pinned reverse merges and readoption.
#![cfg(feature = "test-support")]
#![allow(clippy::unwrap_used, reason = "real Git adoption fixtures")]

use plakous_recursive_subagents::{
    protocol::{
        AdoptCandidateRequest, CandidateId, MessagePurpose, SendAgentMessageRequest,
        SpawnAgentRequest, Tier, TopologyErrorCode, WriterMode,
    },
    tree::{AwaitAgents, ProductAgentTree, TreeStore},
};
use plakous_recursive_subagents_store::SqliteTreeStore;
use plakous_workspace::vcs_fixtures::git_repo;

struct Fixture {
    repo: tempfile::TempDir,
    tree: ProductAgentTree,
    source: CandidateId,
    agent: String,
    path: std::path::PathBuf,
}

fn request(id: &CandidateId, revert: bool) -> AdoptCandidateRequest {
    AdoptCandidateRequest {
        revert,
        candidate_id: id.clone(),
        accept_conflicts: false,
        expected_target: None,
        evidence: None,
    }
}

impl Fixture {
    fn new() -> Self {
        let repo = git_repo();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            "revert-tests",
            "root",
            "owner",
            repo.path().to_path_buf(),
            "tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "worker", "work")
                    .with_fork(),
                None,
            )
            .unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(path.join("WORK.txt"), "adopted delta\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let source = tree
            .attempt(&child.agent_id)
            .unwrap()
            .candidate_id
            .clone()
            .unwrap();
        let delivery = tree
            .timeout_wait_rows_for_test("root", &AwaitAgents::Named(vec![child.agent_id.clone()]))
            .unwrap();
        for mail in delivery.mail {
            tree.acknowledge_mail(&mail.message_id).unwrap();
        }
        tree.adopt_candidate("root", &request(&source, false), None)
            .unwrap();
        Self {
            repo,
            tree,
            source,
            agent: child.agent_id,
            path,
        }
    }

    fn followup(&mut self) -> CandidateId {
        self.tree
            .send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    acknowledge_recovery: None,
                    refresh: None,
                    agent_id: self.agent.clone(),
                    message: "next delta".into(),
                    purpose: Some(MessagePurpose::Followup),
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        std::fs::write(self.path.join("NEXT.txt"), "next delta\n").unwrap();
        self.tree.record_turn_completed(&self.agent).unwrap();
        self.tree.settle_for_test();
        self.tree
            .attempt(&self.agent)
            .unwrap()
            .candidate_id
            .clone()
            .unwrap()
    }
}

#[test]
fn revert_removes_only_the_adopted_delta_keeping_later_edits_and_original_pins() {
    let mut f = Fixture::new();
    std::fs::write(f.repo.path().join("LATER.txt"), "later work\n").unwrap();
    let adopted = f.tree.candidates()[&f.source].clone();
    let receipt = f
        .tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap();
    assert!(receipt.reverted);
    assert!(!f.repo.path().join("WORK.txt").exists());
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("LATER.txt")).unwrap(),
        "later work\n"
    );
    let record = &f.tree.candidates()[&f.source];
    assert_eq!(record.reverted_at.as_ref(), Some(&receipt.target_after));
    assert_eq!(record.reverted_by.as_deref(), Some("root"));
    assert!(record.adopted_by.is_none());
    assert_eq!(record.target_before, adopted.target_before);
    assert_eq!(record.merged_at, adopted.merged_at);
    let namespace = &plakous_workspace::occupancy_vcs::digest_text("revert-tests")[..16];
    for (suffix, commit) in [
        ("before", adopted.target_before.unwrap()),
        ("after", adopted.merged_at.unwrap()),
    ] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(f.repo.path())
            .args([
                "rev-parse",
                &format!("refs/plak/{namespace}/adopted/{}-{suffix}", f.source),
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), commit);
    }
    let tmp = tempfile::tempdir().unwrap();
    let store = SqliteTreeStore::open(&tmp.path().join("tree.sqlite3")).unwrap();
    f.tree
        .acknowledge_revision(store.save_in_custody(&f.tree).unwrap());
    let reopened = ProductAgentTree::reopen(store.load("revert-tests").unwrap().unwrap());
    assert_eq!(reopened.candidates(), f.tree.candidates());
}

#[test]
fn revert_conflict_applies_nothing_and_never_pins_a_conflict_candidate() {
    let mut f = Fixture::new();
    std::fs::write(f.repo.path().join("WORK.txt"), "later overlapping edit\n").unwrap();
    let before = f.tree.candidates().clone();
    let error = f
        .tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::MergeConflict);
    assert!(error.message.contains("WORK.txt"));
    assert!(
        error
            .message
            .contains("reversal conflicts with later edits; nothing was reverted")
    );
    assert_eq!(f.tree.candidates(), &before);
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("WORK.txt")).unwrap(),
        "later overlapping edit\n"
    );
}

#[test]
fn revert_restores_base_and_readoptability() {
    let mut f = Fixture::new();
    let candidate = f.tree.candidates()[&f.source].clone();
    f.tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap();
    assert_eq!(
        f.tree
            .workspace(&candidate.workspace_id)
            .unwrap()
            .base_commit
            .as_ref(),
        Some(&candidate.base_commit)
    );
    let namespace = &plakous_workspace::occupancy_vcs::digest_text("revert-tests")[..16];
    let pin = std::process::Command::new("git")
        .arg("-C")
        .arg(f.repo.path())
        .args([
            "rev-parse",
            &format!("refs/plak/{namespace}/bases/{}", f.agent),
        ])
        .output()
        .unwrap();
    assert!(pin.status.success());
    assert_eq!(
        String::from_utf8(pin.stdout).unwrap().trim(),
        candidate.base_commit
    );
    f.tree
        .adopt_candidate("root", &request(&f.source, false), None)
        .unwrap();
    assert!(f.tree.candidates()[&f.source].reverted_at.is_none());
    assert!(f.tree.candidates()[&f.source].reverted_by.is_none());
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("WORK.txt")).unwrap(),
        "adopted delta\n"
    );
}

#[test]
fn double_revert_explains_how_to_restore_the_candidate() {
    let mut f = Fixture::new();
    let receipt = f
        .tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap();
    let error = f
        .tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
    assert_eq!(
        error.message,
        format!(
            "already reverted at {}; adopt it again to restore it",
            receipt.target_after
        )
    );
}

#[test]
fn revert_refuses_out_of_stack_order_then_accepts_reverse_order() {
    let mut f = Fixture::new();
    let later = f.followup();
    f.tree
        .adopt_candidate("root", &request(&later, false), None)
        .unwrap();
    let error = f
        .tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
    assert!(error.message.contains(later.as_str()));
    f.tree
        .adopt_candidate("root", &request(&later, true), None)
        .unwrap();
    f.tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap();
    assert!(!f.repo.path().join("WORK.txt").exists());
    assert!(!f.repo.path().join("NEXT.txt").exists());
}

#[test]
fn revert_of_pruned_copy_candidate_readopts_through_launch() {
    let mut f = Fixture::new();
    f.tree
        .reclaim_dormant(u64::MAX, &std::collections::BTreeMap::new())
        .unwrap();
    f.tree.settle_for_test();
    assert!(!f.path.exists());
    f.tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap();
    f.tree
        .adopt_candidate("root", &request(&f.source, false), None)
        .unwrap();
    assert!(f.repo.path().join("WORK.txt").exists());
}

#[test]
fn revert_refuses_missing_provenance_without_changing_the_target() {
    let mut f = Fixture::new();
    let namespace = &plakous_workspace::occupancy_vcs::digest_text("revert-tests")[..16];
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(f.repo.path())
        .args([
            "update-ref",
            "-d",
            &format!("refs/plak/{namespace}/adopted/{}-before", f.source),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let error = f
        .tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("WORK.txt")).unwrap(),
        "adopted delta\n"
    );
}
