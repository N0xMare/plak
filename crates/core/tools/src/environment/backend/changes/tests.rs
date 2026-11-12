use std::path::Path;
use std::str::FromStr;

use plakous_model::{CancellationToken, RunId};

use super::*;
use crate::{ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope};

mod hostile;
mod process_stops;
mod unsupported;
mod wire;

#[tokio::test]
async fn diff_output_and_old_or_index_blob_inputs_are_bounded() {
    let root = checkout();
    std::fs::write(root.path().join("file"), "old\n".repeat(70_000)).unwrap();
    git(root.path(), &["add", "file"]);
    git(root.path(), &["commit", "--quiet", "-m", "large base"]);
    std::fs::write(root.path().join("file"), "new\n".repeat(70_000)).unwrap();
    let environment = environment(root.path());
    let observed = status(&environment).await;
    assert_eq!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Refused(ChangesRefusal::OutputLimit)
    );
    std::fs::write(root.path().join("file"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    git(root.path(), &["add", "file"]);
    std::fs::write(root.path().join("file"), "small\n").unwrap();
    let observed = status(&environment).await;
    assert_eq!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Refused(ChangesRefusal::TooLarge)
    );
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn checkout() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    std::fs::write(root.path().join("file"), "before\n").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "--quiet", "-m", "fixture"]);
    root
}

fn environment(root: &Path) -> ExecutionEnvironment {
    ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(
            RunId::from_str("00000000-0000-0000-0000-000000000041").unwrap(),
        ),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap()
}

async fn ask(environment: &ExecutionEnvironment, request: ChangesRequest) -> ChangesAnswer {
    workspace_changes(
        environment,
        request,
        OperationContext::new(CancellationToken::new()),
    )
    .await
    .unwrap()
}

async fn status(environment: &ExecutionEnvironment) -> ChangesStatus {
    let answer = ask(environment, ChangesRequest::status()).await;
    let ChangesAnswer::Status(status) = answer else {
        panic!("{answer:?}");
    };
    status
}

fn diff_request(status: &ChangesStatus, index: usize) -> ChangesRequest {
    ChangesRequest::diff(
        status.repository.clone(),
        status.observation.clone(),
        status.entries[index].clone(),
    )
    .unwrap()
}

#[tokio::test]
async fn status_diff_stale_worktree_and_stale_index() {
    let root = checkout();
    std::fs::write(root.path().join("file"), "after\n").unwrap();
    let environment = environment(root.path());
    let observed = status(&environment).await;
    assert_eq!(observed.entries.len(), 1);
    assert_eq!(observed.entries[0].state, ".M");
    let ChangesAnswer::Diff(ChangesDiff::Patch { text }) =
        ask(&environment, diff_request(&observed, 0)).await
    else {
        panic!("diff");
    };
    assert!(text.contains("-before\n+after"));
    std::fs::write(root.path().join("file"), "third\n").unwrap();
    assert_eq!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Refused(ChangesRefusal::Conflict)
    );
    let observed = status(&environment).await;
    git(root.path(), &["add", "file"]);
    assert_eq!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Refused(ChangesRefusal::Conflict)
    );
    git(root.path(), &["commit", "--quiet", "-m", "staged content"]);
    std::fs::write(root.path().join("file"), "fourth\n").unwrap();
    let observed = status(&environment).await;
    git(
        root.path(),
        &["commit", "--quiet", "--allow-empty", "-m", "new head"],
    );
    assert_eq!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Refused(ChangesRefusal::Conflict)
    );
}

#[tokio::test]
async fn sha256_repositories_use_their_own_read_only_empty_tree_source() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet", "--object-format=sha256"]);
    std::fs::write(root.path().join("file"), "old\n").unwrap();
    git(root.path(), &["add", "file"]);
    git(root.path(), &["commit", "--quiet", "-m", "fixture"]);
    std::fs::write(root.path().join("file"), "new\n").unwrap();
    let environment = environment(root.path());
    let observed = status(&environment).await;
    assert_eq!(observed.observation.head.as_ref().unwrap().len(), 64);
    assert!(matches!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Diff(ChangesDiff::Patch { .. })
    ));
}

#[tokio::test]
async fn discovery_never_climbs_but_explicit_monorepo_is_literal_and_contained() {
    let root = checkout();
    let sub = root.path().join("space[glob]");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("inside"), "inside").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "--quiet", "-m", "subdirectory"]);
    std::fs::write(sub.join("inside"), "changed").unwrap();
    std::fs::write(root.path().join("outside"), "outside").unwrap();
    let environment = environment(&sub);
    assert_eq!(
        ask(&environment, ChangesRequest::status()).await,
        ChangesAnswer::Refused(ChangesRefusal::NotARepository)
    );
    let request = ChangesRequest::status_in_repository(root.path().to_path_buf()).unwrap();
    let ChangesAnswer::Status(status) = ask(&environment, request).await else {
        panic!("ancestor");
    };
    assert_eq!(status.entries.len(), 1);
    assert_eq!(status.entries[0].path, "inside");
    let outside = b"? outside\0";
    assert!(git::parse_status(outside, &sub, root.path()).is_err());
}

#[tokio::test]
async fn untracked_binary_deleted_and_unborn_have_fixed_semantics() {
    let root = checkout();
    std::fs::write(root.path().join("file"), b"binary\0bytes").unwrap();
    let environment = environment(root.path());
    let observed = status(&environment).await;
    assert!(matches!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Diff(ChangesDiff::Binary { .. })
    ));
    std::fs::remove_file(root.path().join("file")).unwrap();
    let observed = status(&environment).await;
    assert_eq!(observed.entries[0].content, ChangesContent::Missing);
    assert!(matches!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Diff(ChangesDiff::Patch { .. })
    ));
    std::fs::write(root.path().join("new"), "new").unwrap();
    let observed = status(&environment).await;
    let index = observed
        .entries
        .iter()
        .position(|entry| entry.state == "??")
        .unwrap();
    assert_eq!(
        ask(&environment, diff_request(&observed, index)).await,
        ChangesAnswer::Refused(ChangesRefusal::VcsUnsupported)
    );
    let unborn = tempfile::tempdir().unwrap();
    git(unborn.path(), &["init", "--quiet"]);
    std::fs::write(unborn.path().join("new"), "new\n").unwrap();
    git(unborn.path(), &["add", "new"]);
    let environment = tests::environment(unborn.path());
    let observed = status(&environment).await;
    assert!(observed.observation.head.is_none());
    assert!(matches!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Diff(ChangesDiff::Patch { .. })
    ));
}

#[tokio::test]
async fn too_large_inputs_and_many_untracked_never_look_clean() {
    let root = checkout();
    std::fs::write(root.path().join("file"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    let environment = environment(root.path());
    let observed = status(&environment).await;
    assert_eq!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Refused(ChangesRefusal::TooLarge)
    );
    for number in 0..2001 {
        std::fs::write(root.path().join(format!("untracked-{number}")), "x").unwrap();
    }
    assert_eq!(
        ask(&environment, ChangesRequest::status()).await,
        ChangesAnswer::Refused(ChangesRefusal::OutputLimit)
    );
}

#[tokio::test]
async fn jj_is_never_invoked_and_colocated_is_explicitly_git_view() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".jj")).unwrap();
    let environment = environment(root.path());
    assert_eq!(
        ask(&environment, ChangesRequest::status()).await,
        ChangesAnswer::Refused(ChangesRefusal::VcsUnsupported)
    );
    git(root.path(), &["init", "--quiet"]);
    assert!(status(&environment).await.repository.git_view);
}

#[test]
fn wire_is_closed_and_validates_constructor_bounds() {
    for value in [
        serde_json::json!({"query": "log"}),
        serde_json::json!({"query": "status", "repository": null, "argv": []}),
        serde_json::json!({"query": "status", "repository": "relative"}),
    ] {
        assert!(serde_json::from_value::<ChangesRequest>(value).is_err());
    }
    assert!(
        serde_json::from_value::<ChangesRequest>(
            serde_json::json!({"query": "status", "repository": null})
        )
        .is_ok()
    );
    for value in ["git version 2.42.9", "git version 1.99.0", "not git"] {
        assert!(!git::supported_version(value));
    }
    assert!(git::supported_version("git version 2.43.0"));
}
