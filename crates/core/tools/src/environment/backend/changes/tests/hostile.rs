use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn mode_drift_conflicts_even_when_porcelain_state_and_content_bytes_are_unchanged() {
    let root = checkout();
    std::fs::write(root.path().join("file"), "changed\n").unwrap();
    let environment = environment(root.path());
    let observed = status(&environment).await;
    std::fs::set_permissions(
        root.path().join("file"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let current = status(&environment).await;
    assert_eq!(observed.observation, current.observation);
    assert_eq!(observed.entries[0].state, current.entries[0].state);
    assert_ne!(observed.entries[0].content, current.entries[0].content);
    assert_eq!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Refused(ChangesRefusal::Conflict)
    );
}

#[test]
fn protected_roots_match_empty_variable_fallbacks_and_fail_closed_without_authority() {
    use crate::environment::backend::changes::repository::protected_roots_from;
    use std::ffi::OsStr;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    assert_eq!(
        protected_roots_from(
            Some(home.as_os_str()),
            Some(OsStr::new("")),
            Some(OsStr::new(""))
        )
        .unwrap(),
        vec![home.join(".plak"), home.join(".local/state/plak")]
    );
    assert!(protected_roots_from(None, None, None).is_err());
    assert!(
        protected_roots_from(Some(home.as_os_str()), None, Some(OsStr::new("relative"))).is_err()
    );
}

#[tokio::test]
async fn missing_promised_objects_cannot_reach_a_network_listener_or_write_objects() {
    let root = checkout();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/repository", listener.local_addr().unwrap());
    git(root.path(), &["config", "remote.origin.url", &url]);
    git(root.path(), &["config", "remote.origin.promisor", "true"]);
    git(root.path(), &["config", "protocol.http.allow", "always"]);
    std::fs::write(root.path().join("file"), "changed\n").unwrap();
    let environment = environment(root.path());
    let observed = status(&environment).await;
    let object = observed.entries[0].head_object.as_deref().unwrap();
    let path = root
        .path()
        .join(".git/objects")
        .join(&object[..2])
        .join(&object[2..]);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Refused(ChangesRefusal::VcsUnavailable)
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!path.exists());
    assert!(
        !root
            .path()
            .join(".git/objects/4b/825dc642cb6eb9a060e54bf8d69288fbee4904")
            .exists()
    );
}

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
    let index = review::index_witness(root.path());
    let observed = status(&environment).await;
    assert!(matches!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Diff(ChangesDiff::Patch { .. })
    ));
    assert!(!marker.exists());
    assert_eq!(review::index_witness(root.path()), index);
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
