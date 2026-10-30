use super::*;
use crate::tree::test_support::*;
use crate::tree::{AwaitAgents, WriterMode};

pub(in crate::tree) fn git_repo() -> tempfile::TempDir {
    let repo = tempdir().unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@local",
            "commit",
            "--allow-empty",
            "-qm",
            "seed",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
    }
    repo
}

#[test]
fn dormant_reclaim_preserves_uncommitted_work_and_candidate() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(path.join("WORK.txt"), "uncommitted\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate = candidate_of(&tree, &child.agent_id);
    let result = tree
        .poll_wait("root", &AwaitAgents::Pending)
        .unwrap()
        .unwrap();
    tree.acknowledge_wait_mail("root", &result);
    tree.mark_wait_delivered("root", &result);
    assert!(tree.dormant(tree.node(&child.agent_id).unwrap()));
    tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
    let settled = tree.settle_for_test();
    assert!(
        !path.exists(),
        "dormant checkout was not reclaimed: {:?}; {:?}; {:?}; {:?}",
        settled.cleanup_failures,
        tree.fault_reason(&child.agent_id),
        tree.workspaces,
        tree.operations.get(&ProductAgentTree::reclamation_id(
            &tree.node(&child.agent_id).unwrap().workspace_id
        ))
    );
    tree.adopt_candidate("root", &adopt_req(&candidate), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("WORK.txt")).unwrap(),
        "uncommitted\n"
    );
}

fn finished() -> (tempfile::TempDir, ProductAgentTree, String, PathBuf) {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree
        .spawn_agent("root", &mint_req(), None)
        .unwrap()
        .agent_id;
    let path = tree.node(&child).unwrap().path.clone();
    std::fs::write(path.join("WORK.txt"), "sealed\n").unwrap();
    tree.record_turn_completed(&child).unwrap();
    tree.settle_for_test();
    let result = tree
        .poll_wait("root", &AwaitAgents::Pending)
        .unwrap()
        .unwrap();
    tree.acknowledge_wait_mail("root", &result);
    tree.mark_wait_delivered("root", &result);
    (repo, tree, child, path)
}

#[test]
fn refused_reclamation_backs_off_per_copy_and_resets_on_generation_change() {
    let (_repo, mut tree, child, _path) = finished();
    let workspace = tree.node(&child).unwrap().workspace_id.clone();
    tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
    let ticket = tree.take_effects().dormancy.pop().unwrap();
    tree.complete_dormancy_at(
        ticket,
        Err(VcsError::mint_unavailable("ignored content retained")),
        0,
    )
    .unwrap_err();
    let mut now = 0;
    for delay in [
        30_000, 60_000, 120_000, 240_000, 480_000, 960_000, 1_920_000, 3_600_000, 3_600_000,
    ] {
        let next = now + delay;
        assert_eq!(tree.reclamation_retry_at(&workspace), next);
        tree.reclaim_dormant(next - 1, &BTreeMap::new()).unwrap();
        assert!(tree.take_effects().dormancy.is_empty());
        // Pressure makes this test independent of the real attempt end time.
        let sizes = BTreeMap::from([(workspace.clone(), u64::MAX)]);
        tree.reclaim_dormant(next, &sizes).unwrap();
        let ticket = tree.take_effects().dormancy.pop().unwrap();
        tree.complete_dormancy_at(
            ticket,
            Err(VcsError::mint_unavailable("ignored content retained")),
            next,
        )
        .unwrap_err();
        now = next;
    }
    tree.workspaces.get_mut(&workspace).unwrap().generation += 1;
    assert_eq!(tree.reclamation_retry_at(&workspace), 0);
    tree.reset_reclamation_backoff(&child);
    assert!(!tree.reclaim_backoff.contains_key(&workspace));
}

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Warnings(Arc<AtomicUsize>);
impl tracing::Subscriber for Warnings {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == "plak.workspace" && *metadata.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
#[test]
fn refusal_detail_is_logged_only_when_it_changes() {
    let (_repo, mut tree, child, _path) = finished();
    let warnings = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(Warnings(Arc::clone(&warnings)), || {
        for (now, detail) in [
            (0, "first refusal"),
            (30_000, "first refusal"),
            (90_000, "changed refusal"),
        ] {
            tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
            let ticket = tree.take_effects().dormancy.pop().unwrap();
            let error = tree
                .complete_dormancy_at(ticket, Err(VcsError::mint_unavailable(detail)), now)
                .unwrap_err();
            assert!(error.message.contains(detail));
        }
    });
    assert_eq!(warnings.load(Ordering::Relaxed), 2);
    let workspace = tree.node(&child).unwrap().workspace_id.clone();
    tree.discard_workspace(
        "root",
        &super::super::WorkspaceIdRequest {
            workspace_id: workspace.clone(),
        },
        None,
    )
    .unwrap();
    assert!(!tree.reclaim_backoff.contains_key(&workspace));
}

#[test]
fn removal_refusal_backoff_survives_resealing_and_internal_generation_bumps() {
    let (_repo, mut tree, child, _path) = finished();
    let workspace = tree.node(&child).unwrap().workspace_id.clone();
    let warnings = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(Warnings(Arc::clone(&warnings)), || {
        for (now, delay) in [(0, 30_000), (30_000, 60_000), (90_000, 120_000)] {
            tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
            let mut seal = tree.take_effects().dormancy.pop().unwrap();
            let result = seal.run(tree.vcs(), &tree.launch_checkout);
            assert!(matches!(result, Ok(DormancyAnswer::Sealed(_))));
            tree.complete_dormancy_at(seal, result, now).unwrap();
            let removal = tree.take_effects().dormancy.pop().unwrap();
            tree.complete_dormancy_at(
                removal,
                Err(VcsError::mint_unavailable(
                    "workspace changed after sealing",
                )),
                now,
            )
            .unwrap_err();
            assert_eq!(tree.reclamation_retry_at(&workspace), now + delay);
        }
    });
    assert_eq!(warnings.load(Ordering::Relaxed), 1);
}

#[test]
fn target_is_retained_after_post_seal_change_or_operational_seal_failure() {
    let (repo, mut tree, _child, path) = finished();
    std::fs::write(path.join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir(path.join("target")).unwrap();
    std::fs::write(path.join("target/cache"), "disposable").unwrap();
    tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
    let mut seal = tree.take_effects().dormancy.pop().unwrap();
    seal.trim_target = true;
    let reference =
        plakous_workspace::occupancy_vcs::candidate_ref(&seal.namespace, &seal.reference_id);
    let lock = repo.path().join(".git").join(format!("{reference}.lock"));
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, "synthetic lock").unwrap();
    assert!(seal.run(tree.vcs(), &tree.launch_checkout).is_err());
    assert!(seal.trim_outcome.is_none());
    assert!(path.join("target/cache").exists());
    std::fs::remove_file(lock).unwrap();
    let result = seal.run(tree.vcs(), &tree.launch_checkout);
    tree.complete_dormancy_at(seal, result, 0).unwrap();
    let mut removal = tree.take_effects().dormancy.pop().unwrap();
    std::fs::write(path.join("WORK.txt"), "concurrent change").unwrap();
    let error = removal.run(tree.vcs(), &tree.launch_checkout).unwrap_err();
    assert_eq!(
        error.reclamation_refusal,
        Some(plakous_workspace::occupancy_vcs::ReclamationRefusal::ChangedAfterSeal)
    );
    assert!(removal.trim_outcome.is_none());
    assert!(path.join("target/cache").exists());
}

#[test]
fn over_budget_retained_copy_trims_target_without_touching_the_refusal() {
    let (_repo, mut tree, child, path) = finished();
    std::fs::write(path.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(path.join(".env"), "synthetic retained content").unwrap();
    std::fs::create_dir(path.join("target")).unwrap();
    std::fs::write(path.join("target/cache"), "disposable").unwrap();
    let workspace = tree.node(&child).unwrap().workspace_id.clone();
    tree.reclaim_dormant(u64::MAX, &BTreeMap::from([(workspace.clone(), u64::MAX)]))
        .unwrap();
    let mut ticket = tree.take_effects().dormancy.pop().unwrap();
    let answer = ticket.run(tree.vcs(), &tree.launch_checkout);
    assert!(answer.is_err());
    assert!(path.join(".env").exists());
    assert!(!path.join("target").exists());
    tree.complete_dormancy_at(ticket, answer, 0).unwrap_err();
    assert_eq!(tree.reclamation_retry_at(&workspace), 30_000);
    assert_eq!(tree.reclaim_trim_epochs.get(&workspace), Some(&1));
}

#[test]
fn target_trim_is_only_enabled_for_an_over_budget_intent() {
    for over_budget in [false, true] {
        let (_repo, mut tree, child, _path) = finished();
        let workspace = tree.node(&child).unwrap().workspace_id.clone();
        let sizes = BTreeMap::from([(workspace, if over_budget { u64::MAX } else { 0 })]);
        tree.reclaim_dormant(u64::MAX, &sizes).unwrap();
        let ticket = tree.take_effects().dormancy.pop().unwrap();
        assert_eq!(ticket.trim_target, over_budget);
    }
}

#[test]
fn dormant_reclaim_crash_cuts_seal_remove_and_rehydrate() {
    for cut in 0..6 {
        let (_repo, mut tree, child, path) = finished();
        std::fs::write(path.join("AFTER.txt"), "late uncommitted\n").unwrap();
        tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
        if cut > 0 {
            let completed = tree.take_effects().run(tree.vcs());
            if cut > 1 {
                tree.complete(completed);
            }
        }
        if cut > 2 {
            let completed = tree.take_effects().run(tree.vcs());
            if cut > 3 {
                tree.complete(completed);
            }
        }
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        reopened
            .reclaim_dormant(u64::MAX, &BTreeMap::new())
            .unwrap();
        assert!(
            reopened.settle_for_test().cleanup_failures.is_empty(),
            "cut {cut}; {:?}",
            reopened.fault_reason(&child)
        );
        assert!(!path.exists(), "cut {cut}");
        reopened.state = super::super::TreeAdmissionState::Live;
        let request = SendAgentMessageRequest {
            refresh: None,
            agent_id: child.clone(),
            message: "continue".into(),
            purpose: Some(super::super::MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        };
        reopened
            .rehydrate_followup("root", &request, "followup")
            .unwrap();
        if cut == 5 {
            let _unpublished = reopened.take_effects().run(reopened.vcs());
            reopened = ProductAgentTree::reopen(reopened.to_persisted());
            reopened
                .reclaim_dormant(u64::MAX, &BTreeMap::new())
                .unwrap();
        }
        assert!(
            reopened.settle_for_test().cleanup_failures.is_empty(),
            "rehydrate cut {cut}"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("AFTER.txt")).unwrap(),
            "late uncommitted\n"
        );
        reopened.state = super::super::TreeAdmissionState::Live;
        reopened.send_agent_message("root", &request, None).unwrap();
        assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Running);
    }
}

#[test]
fn dormant_reclaim_retains_unknown_ignored_secret_and_staged_content() {
    for kind in 0..3 {
        let (_repo, mut tree, _child, path) = finished();
        match kind {
            0 => {
                std::fs::write(path.join(".gitignore"), "private-cache\n").unwrap();
                std::fs::write(path.join("private-cache"), "must retain\n").unwrap();
            }
            1 => {
                std::fs::write(path.join(".env"), "not credentials\n").unwrap();
            }
            _ => {
                std::fs::write(path.join("WORK.txt"), "staged\n").unwrap();
                assert!(
                    std::process::Command::new("git")
                        .args(["add", "WORK.txt"])
                        .current_dir(&path)
                        .status()
                        .unwrap()
                        .success()
                );
                std::fs::write(path.join("WORK.txt"), "working\n").unwrap();
            }
        }
        tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
        assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
        assert!(path.exists());
    }
}

#[test]
fn dormant_reclaim_retains_changes_between_seal_and_delete() {
    let (_repo, mut tree, _child, path) = finished();
    tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
    let completed = tree.take_effects().run(tree.vcs());
    tree.complete(completed);
    std::fs::write(path.join("AFTER.txt"), "must retain\n").unwrap();
    assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
    assert_eq!(
        std::fs::read_to_string(path.join("AFTER.txt")).unwrap(),
        "must retain\n"
    );
}

#[test]
fn dormant_reclaim_disk_pressure_selects_oldest_and_never_shared_live_copy() {
    let (_repo, mut tree, child, path) = finished();
    let workspace = tree.node(&child).unwrap().workspace_id.clone();
    let now = tree.attempt(&child).unwrap().ended_at.unwrap();
    tree.reclaim_dormant(now, &BTreeMap::new()).unwrap();
    assert!(tree.take_effects().is_empty());
    tree.snapshot.workspace_budget_mib = 1;
    tree.reclaim_dormant(now, &BTreeMap::from([(workspace, 2 * 1024 * 1024)]))
        .unwrap();
    assert!(!tree.take_effects().is_empty());
    assert!(path.exists(), "preparation cannot delete the copy");
}
