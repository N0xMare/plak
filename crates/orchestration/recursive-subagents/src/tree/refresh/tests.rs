#![allow(
    clippy::unwrap_used,
    reason = "real Git refresh and crash-cut fixtures"
)]

use super::*;
use crate::protocol::{AdoptCandidateRequest, WriterMode};
use crate::tree::test_support::{candidate_of, mint_req, tree_at};
use crate::tree::{AwaitAgents, MemoryTreeStore, TreeStore};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct Fixture {
    repo: tempfile::TempDir,
    tree: ProductAgentTree,
    agent: String,
    workspace: String,
    path: PathBuf,
    base: String,
}

fn git(path: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn text(path: &Path, args: &[&str]) -> String {
    String::from_utf8(git(path, args))
        .unwrap()
        .trim()
        .to_owned()
}

impl Fixture {
    fn new(conflict: bool) -> Self {
        let repo = plakous_workspace::vcs_fixtures::git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        let base = tree
            .workspace(&child.workspace.id)
            .unwrap()
            .base_commit
            .clone()
            .unwrap();
        std::fs::write(path.join("CHILD.txt"), "child work\n").unwrap();
        if conflict {
            std::fs::write(path.join("SEED.txt"), "child\n").unwrap();
        }
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let delivery = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        tree.acknowledge_wait_mail("root", &delivery);
        if conflict {
            std::fs::write(repo.path().join("SEED.txt"), "parent\n").unwrap();
        } else {
            std::fs::write(repo.path().join("PARENT.txt"), "parent work\n").unwrap();
        }
        Self {
            repo,
            tree,
            agent: child.agent_id,
            workspace: child.workspace.id,
            path,
            base,
        }
    }

    fn request(&self, refresh: Option<bool>) -> SendAgentMessageRequest {
        SendAgentMessageRequest {
            refresh,
            agent_id: self.agent.clone(),
            message: "next task".into(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        }
    }

    fn followup(&mut self, refresh: Option<bool>, jobs: bool) -> super::super::SendMessageOutput {
        let request = self.request(refresh);
        self.tree
            .prepare_followup_refresh("root", &request, "followup", jobs)
            .unwrap();
        assert_eq!(self.tree.settle_for_test().cleanup_failures, Vec::new());
        self.tree
            .send_agent_message("root", &request, Some("followup"))
            .unwrap()
    }

    fn pin(&self) -> String {
        text(
            &self.path,
            &[
                "rev-parse",
                &format!(
                    "refs/plak/{}/bases/{}",
                    &super::super::digest_text("tree1")[..16],
                    self.agent
                ),
            ],
        )
    }
}

#[test]
fn followup_refreshes_live_fork_onto_parent_state_and_reports_to_both_sides() {
    let mut f = Fixture::new(false);
    let old_head = text(&f.path, &["rev-parse", "HEAD"]);
    let receipt = f.followup(None, false);
    let report = receipt.refresh.unwrap();
    assert_eq!(report.status, RefreshStatus::Refreshed);
    let base = report.base.unwrap();
    assert_eq!(
        f.tree
            .workspace(&f.workspace)
            .unwrap()
            .base_commit
            .as_deref(),
        Some(base.as_str())
    );
    assert_eq!(f.pin(), base);
    assert_eq!(
        std::fs::read_to_string(f.path.join("PARENT.txt")).unwrap(),
        "parent work\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.path.join("CHILD.txt")).unwrap(),
        "child work\n"
    );
    assert_eq!(
        text(&f.path, &["show", "-s", "--format=%P", "HEAD"]),
        format!("{old_head} {base}")
    );
    assert_eq!(
        git(&f.path, &["diff", "--cached", "--name-only"]),
        Vec::<u8>::new()
    );
    let mail = f.tree.mail(&receipt.message_id).unwrap();
    assert!(mail.body.contains("Your workspace was refreshed"));
    let again = f
        .tree
        .send_agent_message("root", &f.request(None), Some("followup"))
        .unwrap();
    assert_eq!(again.message_id, receipt.message_id);
}

#[test]
fn refresh_conflict_keeps_the_old_base_and_reports_paths_to_parent_and_child() {
    let mut f = Fixture::new(true);
    let head = text(&f.path, &["rev-parse", "HEAD"]);
    let receipt = f.followup(None, false);
    let report = receipt.refresh.unwrap();
    assert_eq!(report.status, RefreshStatus::Conflict);
    assert!(report.paths.iter().any(|p| p.path == "SEED.txt"));
    assert_eq!(f.pin(), f.base);
    assert_eq!(text(&f.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(f.path.join("SEED.txt")).unwrap(),
        "child\n"
    );
    let mail = f.tree.mail(&receipt.message_id).unwrap();
    assert!(mail.body.contains("SEED.txt"));
    assert!(mail.body.contains("base is unchanged"));
}

#[test]
fn refresh_false_opts_out_without_changing_the_copy() {
    let mut f = Fixture::new(false);
    assert!(f.followup(Some(false), false).refresh.is_none());
    assert_eq!(f.pin(), f.base);
    assert!(!f.path.join("PARENT.txt").exists());
}

#[test]
fn jj_live_fork_followup_reports_refresh_skipped_without_blocking_the_next_attempt() {
    use plakous_workspace::vcs_fixtures::{jj_available, jj_repo};
    assert!(jj_available(), "real jj is required for the refresh law");
    let repo = jj_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(path.join("CHILD.txt"), "child work\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    std::fs::write(repo.path().join("PARENT.txt"), "parent work\n").unwrap();
    let request = SendAgentMessageRequest {
        refresh: None,
        agent_id: child.agent_id.clone(),
        message: "next task".into(),
        purpose: Some(MessagePurpose::Followup),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    };
    tree.prepare_followup_refresh("root", &request, "jj-followup", false)
        .unwrap();
    assert_eq!(tree.settle_for_test().cleanup_failures, Vec::new());
    let receipt = tree
        .send_agent_message("root", &request, Some("jj-followup"))
        .unwrap();
    let report = receipt.refresh.unwrap();
    assert_eq!(report.status, RefreshStatus::Skipped);
    assert!(
        report
            .reason
            .unwrap()
            .contains("refresh is not yet available for jj copies")
    );
    assert!(!path.join("PARENT.txt").exists());
    assert_eq!(
        tree.attempt(&child.agent_id).unwrap().attempt_id.as_str(),
        "c1-a2"
    );
}

#[test]
fn copy_minted_over_candidate_is_never_refreshed_even_when_the_parent_moves() {
    let mut f = Fixture::new(false);
    let source = candidate_of(&f.tree, &f.agent);
    let mut spawn = mint_req();
    spawn.inputs = Some(vec![source.to_string()]);
    let child = f.tree.spawn_agent("root", &spawn, None).unwrap();
    f.agent = child.agent_id;
    f.workspace = child.workspace.id;
    f.path = f.tree.node(&f.agent).unwrap().path.clone();
    f.base = f
        .tree
        .workspace(&f.workspace)
        .unwrap()
        .base_commit
        .clone()
        .unwrap();
    f.tree.record_turn_completed(&f.agent).unwrap();
    f.tree.settle_for_test();
    let error = f
        .tree
        .prepare_followup_refresh("root", &f.request(Some(true)), "explicit", false)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::SchemaMismatch);
    assert!(f.followup(None, false).refresh.is_none());
    assert_eq!(f.pin(), f.base);
    assert!(!f.path.join("PARENT.txt").exists());
}

#[test]
fn refresh_skips_running_jobs_detached_head_and_independent_staged_content() {
    for cause in ["jobs", "detached", "staged"] {
        let mut f = Fixture::new(false);
        if cause == "detached" {
            git(&f.path, &["checkout", "--detach"]);
        }
        if cause == "staged" {
            git(&f.path, &["add", "CHILD.txt"]);
            std::fs::write(f.path.join("CHILD.txt"), "newer unstaged version\n").unwrap();
        }
        let receipt = f.followup(None, cause == "jobs");
        assert_eq!(receipt.refresh.unwrap().status, RefreshStatus::Skipped);
        assert_eq!(f.pin(), f.base);
        assert!(!f.path.join("PARENT.txt").exists());
        if cause == "staged" {
            assert_eq!(text(&f.path, &["show", ":CHILD.txt"]), "child work");
            assert_eq!(
                std::fs::read_to_string(f.path.join("CHILD.txt")).unwrap(),
                "newer unstaged version\n"
            );
        }
    }
}

#[test]
fn older_candidate_adopts_after_refresh_without_moving_the_refreshed_base_or_pin() {
    let mut f = Fixture::new(false);
    let old = candidate_of(&f.tree, &f.agent);
    let receipt = f.followup(None, false);
    let base = receipt.refresh.unwrap().base.unwrap();
    f.tree.record_turn_completed(&f.agent).unwrap();
    f.tree.settle_for_test();
    f.tree
        .adopt_candidate(
            "root",
            &AdoptCandidateRequest {
                revert: false,
                candidate_id: old,
                accept_conflicts: false,
                expected_target: None,
                evidence: None,
            },
            None,
        )
        .unwrap();
    assert_eq!(
        f.tree
            .workspace(&f.workspace)
            .unwrap()
            .base_commit
            .as_deref(),
        Some(base.as_str())
    );
    assert_eq!(f.pin(), base);
    assert!(f.repo.path().join("CHILD.txt").exists());
    assert!(f.repo.path().join("PARENT.txt").exists());
}

#[test]
fn independent_staged_versions_with_whitespace_paths_are_retained() {
    let mut f = Fixture::new(false);
    for name in [" earlier.txt", " leading.txt"] {
        std::fs::write(f.path.join(name), "committed\n").unwrap();
    }
    git(&f.path, &["add", " earlier.txt", " leading.txt"]);
    git(&f.path, &["commit", "-m", "path fixture"]);
    std::fs::write(f.path.join(" earlier.txt"), "unstaged\n").unwrap();
    std::fs::write(f.path.join(" leading.txt"), "staged\n").unwrap();
    git(&f.path, &["add", " leading.txt"]);
    std::fs::write(f.path.join(" leading.txt"), "newer unstaged\n").unwrap();
    let receipt = f.followup(None, false);
    assert_eq!(receipt.refresh.unwrap().status, RefreshStatus::Skipped);
    assert_eq!(text(&f.path, &["show", ": leading.txt"]), "staged");
    assert_eq!(
        std::fs::read_to_string(f.path.join(" leading.txt")).unwrap(),
        "newer unstaged\n"
    );
    assert_eq!(f.pin(), f.base);
}

#[test]
fn snapshot_copies_keep_their_base_and_explicit_refresh_is_refused() {
    let mut f = Fixture::new(false);
    let child = f
        .tree
        .spawn_agent(
            "root",
            &crate::protocol::SpawnAgentRequest::test_review().with_fork(),
            None,
        )
        .unwrap();
    f.agent = child.agent_id;
    f.workspace = child.workspace.id;
    f.path = f.tree.node(&f.agent).unwrap().path.clone();
    f.base = f
        .tree
        .workspace(&f.workspace)
        .unwrap()
        .base_commit
        .clone()
        .unwrap();
    f.tree.record_turn_completed(&f.agent).unwrap();
    f.tree.settle_for_test();
    std::fs::write(f.repo.path().join("LATER.txt"), "new parent state\n").unwrap();
    let error = f
        .tree
        .prepare_followup_refresh("root", &f.request(Some(true)), "explicit", false)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::SchemaMismatch);
    assert!(f.followup(None, false).refresh.is_none());
    assert_eq!(f.pin(), f.base);
    assert!(!f.path.join("LATER.txt").exists());
}

#[test]
fn refresh_recovery_retains_diverged_work_and_its_pending_intent() {
    let mut f = Fixture::new(false);
    let store = MemoryTreeStore::new();
    f.tree
        .prepare_followup_refresh("root", &f.request(None), "followup", false)
        .unwrap();
    let prepared = f.tree.take_effects().run(f.tree.vcs());
    assert_eq!(f.tree.complete(prepared).cleanup_failures, Vec::new());
    f.tree.acknowledge_revision(store.save(&f.tree).unwrap());
    std::fs::write(f.path.join("CHILD.txt"), "operator's newer work\n").unwrap();
    f.tree = ProductAgentTree::reopen(store.load("tree1").unwrap().unwrap());
    let attach = f.tree.take_attach_effects();
    let settled = f.tree.complete(attach.run(f.tree.vcs()));
    assert!(f.tree.refresh_pending(&f.workspace));
    assert!(
        settled
            .cleanup_failures
            .iter()
            .any(|error| error.code == TopologyErrorCode::WorkspaceRecoveryRequired)
    );
    assert_eq!(
        std::fs::read_to_string(f.path.join("CHILD.txt")).unwrap(),
        "operator's newer work\n"
    );
    assert_eq!(f.pin(), f.base);
    assert_eq!(
        f.tree.node(&f.agent).unwrap().status,
        crate::protocol::NodeStatus::RecoveryRequired
    );
}

#[test]
fn live_fork_refresh_crash_cuts_reconcile_only_exact_git_states() {
    for cut in ["intent", "applied", "branch", "pin"] {
        refresh_cut(cut);
    }
}

#[test]
fn a_followup_reconciles_a_failed_refresh_after_the_copy_is_restored_to_a_known_state() {
    let mut f = Fixture::new(false);
    f.tree
        .prepare_followup_refresh("root", &f.request(None), "followup", false)
        .unwrap();
    let prepared = f.tree.take_effects().run(f.tree.vcs());
    assert_eq!(f.tree.complete(prepared).cleanup_failures, Vec::new());
    let ticket = f.tree.take_effects().refreshes.remove(0);
    std::fs::write(f.path.join("CHILD.txt"), "operator edit\n").unwrap();
    let answer = ticket.run(f.tree.vcs());
    let error = f.tree.complete_refresh(ticket, answer).unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
    assert!(!f.tree.inflight_refreshes.contains(&f.workspace));
    std::fs::write(f.path.join("CHILD.txt"), "child work\n").unwrap();
    let receipt = f.followup(None, false);
    assert_eq!(receipt.refresh.unwrap().status, RefreshStatus::Skipped);
    assert!(!f.tree.refresh_pending(&f.workspace));
    assert_eq!(f.pin(), f.base);
}

#[test]
fn an_inflight_refresh_excludes_another_followup_and_candidate_adoption() {
    let mut f = Fixture::new(false);
    f.tree
        .prepare_followup_refresh("root", &f.request(None), "followup", false)
        .unwrap();
    let source = candidate_of(&f.tree, &f.agent);
    let error = f
        .tree
        .prepare_followup_refresh("root", &f.request(None), "other", false)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
    assert_eq!(f.tree.effects.refreshes.len(), 1);
    let error = f
        .tree
        .adopt_candidate(
            "root",
            &AdoptCandidateRequest {
                revert: false,
                candidate_id: source,
                accept_conflicts: false,
                expected_target: None,
                evidence: None,
            },
            None,
        )
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
    assert!(!f.path.join("PARENT.txt").exists());
}

#[test]
fn a_prepared_refresh_binds_the_original_followup_brief_before_send_admission() {
    let mut f = Fixture::new(false);
    f.tree
        .prepare_followup_refresh("root", &f.request(None), "followup", false)
        .unwrap();
    assert_eq!(f.tree.settle_for_test().cleanup_failures, Vec::new());
    let mut changed = f.request(None);
    changed.message.push_str(" with a different brief");
    let error = f
        .tree
        .prepare_followup_refresh("root", &changed, "followup", false)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::OperationRecoveryRequired);
}

#[test]
fn completed_refresh_receipt_survives_an_intervening_followup_before_send_admission() {
    let mut f = Fixture::new(false);
    let original = f.request(None);
    f.tree
        .prepare_followup_refresh("root", &original, "original", false)
        .unwrap();
    assert_eq!(f.tree.settle_for_test().cleanup_failures, Vec::new());
    let first_base = f
        .tree
        .refresh_report(&f.workspace, Some("original"))
        .unwrap()
        .base;
    let other = f.request(None);
    f.tree
        .prepare_followup_refresh("root", &other, "other", false)
        .unwrap();
    assert_eq!(f.tree.settle_for_test().cleanup_failures, Vec::new());
    f.tree
        .send_agent_message("root", &other, Some("other"))
        .unwrap();
    f.tree.record_turn_completed(&f.agent).unwrap();
    f.tree.settle_for_test();
    std::fs::write(
        f.repo.path().join("LATER.txt"),
        "not part of original refresh\n",
    )
    .unwrap();
    f.tree
        .prepare_followup_refresh("root", &original, "original", false)
        .unwrap();
    assert_eq!(f.tree.settle_for_test().cleanup_failures, Vec::new());
    let receipt = f
        .tree
        .send_agent_message("root", &original, Some("original"))
        .unwrap();
    let report = receipt.refresh.unwrap();
    assert_eq!(report.status, RefreshStatus::Refreshed);
    assert_eq!(report.base, first_base);
    assert!(!f.path.join("LATER.txt").exists());
}

fn refresh_cut(cut: &str) {
    let mut f = Fixture::new(false);
    let store = MemoryTreeStore::new();
    f.tree
        .prepare_followup_refresh("root", &f.request(None), "followup", false)
        .unwrap();
    f.tree.acknowledge_revision(store.save(&f.tree).unwrap());
    let prepared = f.tree.take_effects().run(f.tree.vcs());
    assert_eq!(f.tree.complete(prepared).cleanup_failures, Vec::new());
    f.tree.acknowledge_revision(store.save(&f.tree).unwrap());
    let ticket = f.tree.take_effects().refreshes.remove(0);
    let Step::Apply(plan) = &ticket.step else {
        panic!("published apply plan")
    };
    let expected_base = if cut == "intent" {
        f.base.clone()
    } else {
        plan.parent.clone()
    };
    if cut != "intent" {
        let patch = git(
            &f.path,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                &plan.child_content,
                &plan.merged_content,
            ],
        );
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&f.path)
            .arg("apply")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&patch).unwrap();
        assert!(child.wait().unwrap().success());
    }
    if cut == "branch" || cut == "pin" {
        git(
            &f.path,
            &[
                "update-ref",
                &format!("refs/heads/{}", plan.request.branch),
                &plan.merge_commit,
                &plan.head_old,
            ],
        );
    }
    if cut == "pin" {
        git(&f.path, &["read-tree", &plan.merge_commit]);
        git(
            &f.path,
            &[
                "update-ref",
                &format!(
                    "refs/plak/{}/bases/{}",
                    &plan.request.namespace[..16],
                    plan.request.agent
                ),
                &plan.parent,
                &plan.request.base,
            ],
        );
    }
    f.tree = ProductAgentTree::reopen(store.load("tree1").unwrap().unwrap());
    assert_eq!(f.tree.settle_for_test().cleanup_failures, Vec::new());
    assert!(!f.tree.refresh_pending(&f.workspace), "{cut}");
    assert_eq!(f.pin(), expected_base, "{cut}");
    assert_eq!(
        f.tree
            .workspace(&f.workspace)
            .unwrap()
            .base_commit
            .as_deref(),
        Some(expected_base.as_str()),
        "{cut}"
    );
    assert_eq!(f.path.join("PARENT.txt").exists(), cut != "intent", "{cut}");
    assert_eq!(
        std::fs::read_to_string(f.path.join("CHILD.txt")).unwrap(),
        "child work\n"
    );
    assert_eq!(
        git(&f.path, &["diff", "--cached", "--name-only"]),
        Vec::<u8>::new()
    );
}
