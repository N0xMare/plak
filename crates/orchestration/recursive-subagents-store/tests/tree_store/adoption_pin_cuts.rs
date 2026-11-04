//! Copy-base pins mirror committed workspace rows across adoption commit cuts.
#![cfg(feature = "test-support")]
#![allow(
    clippy::unwrap_used,
    reason = "deterministic real-VCS commit-cut fixtures"
)]

use plakous_recursive_subagents::{
    protocol::{AdoptCandidateRequest, SpawnAgentRequest, Tier, WorkspaceStatus, WriterMode},
    tools::TreeDriver,
    tree::{Effects, ProductAgentTree, TreeStore},
};
use plakous_recursive_subagents_store::SqliteTreeStore;
use plakous_workspace::{
    git_redirect_fence,
    occupancy_vcs::{SystemVcs, copy_base_ref, digest_text},
    vcs_fixtures::{git_repo, jj_available, jj_repo},
};
use std::{
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    BeforeLanding,
    AfterLanding,
    BeforeRecordCommit,
    AfterRecordCommit,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_adoption_base_pin_commit_cuts_follow_the_committed_row() {
    for cut in [
        Cut::BeforeLanding,
        Cut::AfterLanding,
        Cut::BeforeRecordCommit,
        Cut::AfterRecordCommit,
    ] {
        adoption_cut(false, cut).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jj_adoption_base_pin_commit_cuts_allow_hosted_fence_construction() {
    if !jj_available() {
        return;
    }
    for cut in [
        Cut::BeforeLanding,
        Cut::AfterLanding,
        Cut::BeforeRecordCommit,
        Cut::AfterRecordCommit,
    ] {
        adoption_cut(true, cut).await;
    }
}

async fn adoption_cut(jj: bool, cut: Cut) {
    let repo = if jj { jj_repo() } else { git_repo() };
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteTreeStore::open(&tmp.path().join("tree.sqlite3")).unwrap());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        "pin-cuts",
        "root",
        "owner",
        repo.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("implement", Tier::Mutate, "worker", "work").with_fork(),
            None,
        )
        .unwrap();
    let path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(path.join("WORK.txt"), "candidate work\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let candidate = tree
        .attempt(&child.agent_id)
        .unwrap()
        .candidate_id
        .clone()
        .unwrap();
    tree.acknowledge_revision(store.save_in_custody(&tree).unwrap());
    tree.begin_adopt_for_test(
        "root",
        &AdoptCandidateRequest {
            revert: false,
            accept_conflicts: false,
            candidate_id: candidate.clone(),
            expected_target: None,
            evidence: None,
        },
    )
    .unwrap();
    tree.acknowledge_revision(store.save_in_custody(&tree).unwrap());
    let mut effects = tree.take_effects();
    let ticket = effects.adopts.remove(0);
    assert!(effects.is_empty());
    let old_base = row_base(&tree, &child.workspace.id);
    if cut != Cut::BeforeLanding {
        let landing =
            plakous_recursive_subagents::tree::block_on_for_test(ticket.land(&SystemVcs)).unwrap();
        assert_ne!(read_base(repo.path(), jj, &child.agent_id), old_base);
        if jj {
            assert!(git_redirect_fence::install_for_copy(&path, Some(&old_base)).is_err());
        }
        if matches!(cut, Cut::BeforeRecordCommit | Cut::AfterRecordCommit) {
            tree.finish_adopt(&ticket, Ok(landing)).unwrap();
        }
        if cut == Cut::AfterRecordCommit {
            tree.acknowledge_revision(store.save_in_custody(&tree).unwrap());
        }
    }
    // Discard the unpublished in-memory outcome, exactly as a process loss does.
    drop(tree);
    let mut reopened = ProductAgentTree::reopen(store.load("pin-cuts").unwrap().unwrap());
    let expected = row_base(&reopened, &child.workspace.id);
    let retry = (!jj && cut == Cut::AfterLanding).then_some(AdoptCandidateRequest {
        revert: false,
        accept_conflicts: false,
        candidate_id: candidate,
        expected_target: None,
        evidence: None,
    });
    // Dropping the first mutation's effects cannot lose attachment recovery.
    drop(reopened.take_effects());
    reopened.acknowledge_revision(store.save_in_custody(&reopened).unwrap());
    let driver = TreeDriver::new(
        Arc::new(Mutex::new(reopened)),
        store.clone(),
        Arc::new(SystemVcs),
        None,
    );
    driver.reconcile_attachment().await.unwrap();
    assert_eq!(
        read_base(repo.path(), jj, &child.agent_id),
        expected,
        "{cut:?}"
    );
    assert!(
        git_redirect_fence::install_for_copy(&path, Some(&expected)).is_ok(),
        "{cut:?}"
    );
    assert_reopened_status(&store, &child.workspace.id, cut);
    driver.settle(Effects::default()).await.unwrap();
    assert_eq!(read_base(repo.path(), jj, &child.agent_id), expected);
    if let Some(request) = retry {
        assert_retry_lands(&driver, repo.path(), &child, &request, &expected).await;
    }
}

fn assert_reopened_status(store: &SqliteTreeStore, workspace: &str, cut: Cut) {
    let persisted = store.load("pin-cuts").unwrap().unwrap();
    assert_eq!(
        persisted.rows().workspace(workspace).unwrap().status,
        if cut == Cut::AfterRecordCommit {
            WorkspaceStatus::Adopted
        } else {
            WorkspaceStatus::Adopting
        }
    );
}

async fn assert_retry_lands(
    driver: &TreeDriver,
    launch: &Path,
    child: &plakous_recursive_subagents::tree::SpawnAgentOutput,
    request: &AdoptCandidateRequest,
    expected: &str,
) {
    let (admitted, landed) = driver
        .mutate_and_settle({
            let request = request.clone();
            move |tree| tree.begin_adopt_for_test("root", &request)
        })
        .await
        .unwrap();
    admitted.unwrap();
    assert!(landed.adopts[0].is_ok());
    let published = driver
        .read(|tree| row_base(tree, &child.workspace.id))
        .unwrap();
    assert_ne!(published, expected);
    driver.settle(Effects::default()).await.unwrap();
    assert_eq!(
        read_base(launch, false, &child.agent_id),
        published,
        "an unrelated settle cannot requeue restoration over a retried landing"
    );
}

fn row_base(tree: &ProductAgentTree, workspace: &str) -> String {
    tree.workspace(workspace)
        .unwrap()
        .base_commit
        .clone()
        .unwrap()
}

fn read_base(launch: &Path, jj: bool, agent: &str) -> String {
    let mut command = Command::new("git");
    if jj {
        let output = Command::new("jj")
            .args(["--no-pager", "git", "root"])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(launch)
            .output()
            .unwrap();
        assert!(output.status.success());
        command
            .arg("--git-dir")
            .arg(String::from_utf8(output.stdout).unwrap().trim());
    } else {
        command.arg("-C").arg(launch);
    }
    let output = command
        .args(["rev-parse", &copy_base_ref(&digest_text("pin-cuts"), agent)])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
