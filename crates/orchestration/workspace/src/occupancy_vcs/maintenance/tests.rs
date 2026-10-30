fn target_fixture() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir(repo.path().join("target")).unwrap();
    std::fs::write(repo.path().join("target/cache"), "disposable").unwrap();
    repo
}

#[test]
fn target_trim_retains_tracked_case_aliases_and_nested_repositories() {
    let repo = target_fixture();
    assert!(
        std::process::Command::new("git")
            .args(["config", "core.ignorecase", "true"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::rename(repo.path().join("target"), repo.path().join("Target")).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["add", "-f", "Target/cache"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(!super::trim_disposable_target(repo.path()).unwrap());
    assert!(repo.path().join("Target/cache").exists());
    for worktree_marker in [false, true] {
        let repo = target_fixture();
        let nested = repo.path().join("target/nested");
        std::fs::create_dir(&nested).unwrap();
        if worktree_marker {
            std::fs::write(nested.join(".git"), "gitdir: synthetic-worktree").unwrap();
        } else {
            assert!(
                std::process::Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(&nested)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert!(!super::trim_disposable_target(repo.path()).unwrap());
        assert!(nested.join(".git").exists());
    }
}

#[test]
fn target_trim_removes_only_ignored_untracked_real_directory() {
    let repo = target_fixture();
    assert!(super::trim_disposable_target(repo.path()).unwrap());
    assert!(!repo.path().join("target").exists());
    for tracked in [false, true] {
        let repo = target_fixture();
        if tracked {
            assert!(
                std::process::Command::new("git")
                    .args(["add", "-f", "target/cache"])
                    .current_dir(repo.path())
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            std::fs::write(repo.path().join(".gitignore"), "").unwrap();
        }
        assert!(!super::trim_disposable_target(repo.path()).unwrap());
        assert!(repo.path().join("target/cache").exists());
    }
}

#[test]
fn target_trim_never_follows_a_symlink_or_removes_secret_content() {
    let repo = target_fixture();
    std::fs::write(repo.path().join("target/.env"), "synthetic").unwrap();
    assert!(!super::trim_disposable_target(repo.path()).unwrap());
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("sentinel"), "retained").unwrap();
    std::fs::remove_dir_all(repo.path().join("target")).unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.path().join("target")).unwrap();
    assert!(!super::trim_disposable_target(repo.path()).unwrap());
    assert!(outside.path().join("sentinel").exists());
}

#[test]
fn read_only_jj_probe_refuses_without_opening_or_snapshotting() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".jj")).unwrap();
    let error = super::dirty(root.path()).unwrap_err();
    assert!(
        error
            .detail
            .contains("read-only JJ changes inspection unavailable")
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
