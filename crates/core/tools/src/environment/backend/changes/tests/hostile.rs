use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn protected_workspace_git_and_linked_common_directories_refuse_in_both_directions() {
    use crate::environment::backend::changes::repository::identify;
    let root = checkout();
    let root_path = root.path().canonicalize().unwrap();
    for excluded in [
        root_path.clone(),
        root_path.join("state"),
        root_path.parent().unwrap().to_path_buf(),
        root_path.join(".git"),
    ] {
        assert_eq!(
            identify(&root_path, None, &[excluded]),
            Err(ChangesRefusal::ProtectedPath)
        );
    }
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    git(
        root.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
        ],
    );
    let linked = linked.canonicalize().unwrap();
    assert!(identify(&linked, None, &[]).is_ok());
    assert_eq!(
        identify(&linked, None, &[root_path.join(".git")]),
        Err(ChangesRefusal::ProtectedPath)
    );
    // A protected subtree of the common directory is excluded too.
    assert_eq!(
        identify(&linked, None, &[root_path.join(".git/subscriptions")]),
        Err(ChangesRefusal::ProtectedPath)
    );
}

#[tokio::test]
async fn hostile_filters_drivers_hooks_helpers_and_promisor_do_not_run() {
    let root = checkout();
    let marker = root.path().join("command-marker");
    let payload = format!("sh -c 'touch {}; exit 1'", marker.display());
    // Commit the attributes before installing any hostile command.
    std::fs::write(
        root.path().join(".gitattributes"),
        "* filter=hostile diff=hostile\n",
    )
    .unwrap();
    git(root.path(), &["add", ".gitattributes"]);
    git(root.path(), &["commit", "--quiet", "-m", "attributes"]);
    for key in [
        "filter.hostile.clean",
        "filter.hostile.process",
        "diff.hostile.textconv",
        "diff.external",
        "core.fsmonitor",
        "credential.helper",
        "core.sshCommand",
    ] {
        git(root.path(), &["config", key, &payload]);
    }
    git(root.path(), &["config", "filter.hostile.required", "true"]);
    git(
        root.path(),
        &[
            "config",
            "remote.origin.url",
            "https://127.0.0.1:9/unreachable",
        ],
    );
    git(root.path(), &["config", "remote.origin.promisor", "true"]);
    std::fs::write(
        root.path().join(".git/hooks/post-index-change"),
        format!("#!/bin/sh\n{payload}\n"),
    )
    .unwrap();
    std::fs::set_permissions(
        root.path().join(".git/hooks/post-index-change"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    // Same-length contents are deliberately stat-dirty, requiring conversion
    // selection even when status cannot short circuit on file length.
    std::fs::write(root.path().join("file"), "AFTER!\n").unwrap();
    let environment = environment(root.path());
    let observed = status(&environment).await;
    assert!(matches!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Diff(ChangesDiff::Patch { .. })
    ));
    assert!(!marker.exists());
    assert!(
        root.path()
            .join(".git/index.lock")
            .symlink_metadata()
            .is_err()
    );
}

#[tokio::test]
async fn disagreeing_worktree_nonempty_info_attributes_and_symlink_content_refuse() {
    let root = checkout();
    let elsewhere = tempfile::tempdir().unwrap();
    git(
        root.path(),
        &[
            "config",
            "core.worktree",
            elsewhere.path().to_str().unwrap(),
        ],
    );
    let environment = environment(root.path());
    assert_eq!(
        ask(&environment, ChangesRequest::status()).await,
        ChangesAnswer::Refused(ChangesRefusal::UnsafeRepository)
    );
    git(root.path(), &["config", "--unset", "core.worktree"]);
    std::fs::write(
        root.path().join(".git/info/attributes"),
        "* filter=hostile\n",
    )
    .unwrap();
    assert_eq!(
        ask(&environment, ChangesRequest::status()).await,
        ChangesAnswer::Refused(ChangesRefusal::UnsafeRepository)
    );
    std::fs::remove_file(root.path().join(".git/info/attributes")).unwrap();
    std::fs::remove_file(root.path().join("file")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), root.path().join("file")).unwrap();
    let observed = status(&environment).await;
    assert_eq!(observed.entries[0].content, ChangesContent::Unsupported);
    assert_eq!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Refused(ChangesRefusal::VcsUnsupported)
    );
}
