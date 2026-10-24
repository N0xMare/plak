use std::process::{Command, Output};

use super::*;
use tempfile::tempdir;

fn git_repo() -> tempfile::TempDir {
    git_repo_with_init(&["init", "-b", "main"])
}

#[test]
fn copy_layout_matches_only_the_leaf_parent_and_grandparent() {
    for (path, expected) in [
        ("launch/.plak/worktrees/c1", Some(SessionVcs::Git)),
        ("launch/.plak/workspaces/c1", Some(SessionVcs::Jj)),
        ("launch/.plak/workspaces/c1/nested", None),
        ("launch/.plak/worktrees/c1/other/c2", None),
        ("launch/.plak/workspaces", None),
        ("launch/workspaces/c1", None),
    ] {
        assert_eq!(copy_layout_kind(Path::new(path)), expected, "{path}");
        assert_eq!(
            is_jj_copy_root(Path::new(path)),
            expected == Some(SessionVcs::Jj),
            "{path}"
        );
    }
}

fn git_repo_with_init(init: &[&str]) -> tempfile::TempDir {
    let root = tempdir().unwrap();
    for args in [
        init.to_vec(),
        vec!["config", "user.email", "plak@example.test"],
        vec!["config", "user.name", "plak"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        assert!(
            Command::new("git")
                .args(&args)
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
    for args in [vec!["add", "SEED.txt"], vec!["commit", "-m", "seed"]] {
        assert!(
            Command::new("git")
                .args(&args)
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
    }
    root
}

fn jj_available() -> bool {
    if !cfg!(feature = "jj-lib") {
        return false;
    }
    Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn jj_repo() -> tempfile::TempDir {
    jj_repo_with_init_args(&["git", "init"])
}

fn jj_repo_with_init_args(init_args: &[&str]) -> tempfile::TempDir {
    let root = tempdir().unwrap();
    for args in [init_args, &["commit", "-m", "seed"]] {
        if args[0] == "commit" {
            std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
        }
        assert!(
            Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
                .args(["--no-pager"])
                .args([
                    "--config",
                    "user.name=plak",
                    "--config",
                    "user.email=plak@local",
                ])
                .args(args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(root.path())
                .status()
                .unwrap()
                .success(),
            "jj {args:?} failed"
        );
    }
    root
}

/// With no configured identity, git derives a default email from the
/// host's FQDN; under musl without a network that lookup stalls every
/// reflog write for the resolver timeout. Plak's steps supply a fallback.
#[test]
fn ref_writes_without_a_configured_identity_use_the_fallback_email() {
    let root = tempdir().unwrap();
    let home = tempdir().unwrap();
    let isolated = |args: &[&str]| {
        let mut command = git_command(root.path());
        command
            .args(args)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1");
        assert!(command.status().unwrap().success(), "git {args:?}");
    };
    isolated(&["init", "-q", "-b", "main"]);
    isolated(&[
        "-c",
        "user.name=seed",
        "-c",
        "user.email=seed@local",
        "commit",
        "--allow-empty",
        "-qm",
        "seed",
    ]);
    isolated(&["update-ref", "-m", "probe", "refs/heads/probe", "HEAD"]);
    let reflog = std::fs::read_to_string(root.path().join(".git/logs/refs/heads/probe")).unwrap();
    let expected = std::env::var("EMAIL").unwrap_or_else(|_| FALLBACK_EMAIL.to_owned());
    assert!(reflog.contains(&format!("<{expected}>")), "{reflog}");
}

/// A `plak/c1` branch an earlier tree left behind does not block a new
/// `c1` fork.
#[test]
fn a_stale_plak_branch_does_not_block_a_fork_mint() {
    let repo = git_repo();
    assert!(
        Command::new("git")
            .args(["branch", "plak/c1"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    assert_eq!(minted.branch.as_deref(), Some("plak/c1"));
    assert!(minted.path.join("SEED.txt").is_file());
}

/// A crashed first tree can retry cleanup after its `c1` was removed and
/// a second tree minted its own `c1`. Tree-scoped copy ids make those
/// physical resources distinct, so the stale retry cannot remove the
/// second tree's worktree or branch.
#[test]
fn stale_tree_cleanup_cannot_remove_another_trees_reused_child_id() {
    let repo = git_repo();
    let first_id = format!("{}-c1", digest_text("tree-first"));
    let second_id = format!("{}-c1", digest_text("tree-second"));
    let first = SystemVcs
        .mint_from(
            repo.path(),
            &first_id,
            &first_id,
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    SystemVcs.unmint(repo.path(), &first.path).unwrap();

    let second = SystemVcs
        .mint_from(
            repo.path(),
            &second_id,
            &second_id,
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    assert_ne!(first.path, second.path);
    // A stale repeat of the first tree's cleanup finds its copy already
    // gone and succeeds without touching the second tree's copy.
    SystemVcs.unmint(repo.path(), &first.path).unwrap();
    assert!(second.path.join("SEED.txt").is_file());
    let branch = second.branch.as_deref().unwrap();
    assert!(
        git_run(
            repo.path(),
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}")
            ]
        )
        .unwrap()
        .status
        .success()
    );
}

/// Concurrent mints in one jj repo must each leave a
/// *workspace* of the launch repo — `.jj/repo` a pointer file — never a
/// standalone repo, and none of them may be refused because a sibling was
/// adding a workspace at the same instant.
#[test]
fn concurrent_jj_mints_all_yield_workspace_pointer_copies() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let launch = repo.path().to_path_buf();
    let minted: Vec<_> = ["c1", "c2", "c3", "c4"]
        .into_iter()
        .map(|agent_id| {
            let launch = launch.clone();
            std::thread::spawn(move || {
                mint_worktree_from(&launch, agent_id, &launch, WorkspaceMode::Fork, None)
                    .map(|copy| copy.path)
            })
        })
        .collect();
    for handle in minted {
        let path = handle
            .join()
            .unwrap()
            .unwrap_or_else(|error| panic!("concurrent mint refused: {}", error.detail));
        let pointer = path.join(".jj").join("repo");
        assert!(
            pointer.is_file(),
            "{} is not a workspace pointer of the launch repo",
            pointer.display()
        );
        assert!(jj_detached_copy(&path).is_none());
        // The copy is a workspace, so the launch repo can read it.
        assert!(jj_snapshot(&path).is_ok());
    }
}

/// A copy whose `.jj/repo` is a *directory* is a
/// standalone repo, not a workspace of the launch repo. Every id it
/// reports is foreign, so it is refused by name rather than snapshotted,
/// compared against a base, or sealed as diverged.
#[test]
fn a_standalone_jj_repo_in_a_minted_copy_is_refused_by_name() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let launch = repo.path().to_path_buf();
    let copy = mint_worktree_from(&launch, "c1", &launch, WorkspaceMode::Fork, None).unwrap();
    let base = copy.base.clone();
    std::fs::rename(copy.path.join(".jj"), copy.path.join(".jj.bak")).unwrap();
    assert!(
        Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
            .args(["--no-pager", "git", "init"])
            .args([
                "--config",
                "user.name=plak",
                "--config",
                "user.email=plak@local",
            ])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(&copy.path)
            .status()
            .unwrap()
            .success()
    );
    assert!(copy.path.join(".jj").join("repo").is_dir());
    let error = jj_snapshot(&copy.path).unwrap_err();
    assert_eq!(error.kind, VcsErrorKind::MintUnavailable);
    assert!(
        error.detail.contains("not a workspace of the launch repo") && error.detail.contains("c1"),
        "{}",
        error.detail
    );
    let error = jj_diverged_from_base(&copy.path, base.change_id.as_deref(), base.commit.as_str())
        .unwrap_err();
    assert!(
        error.detail.contains("standalone jj repo"),
        "{}",
        error.detail
    );
}

#[test]
fn jj_candidate_resolution_uses_immutable_commit_identity() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let saved = jj_snapshot(repo.path()).unwrap();
    std::fs::write(repo.path().join("later.txt"), "later").unwrap();
    let current = jj_snapshot(repo.path()).unwrap();
    assert_ne!(saved.commit, current.commit);
    assert_eq!(
        crate::native_jj::resolve(repo.path(), &saved.commit),
        Some(saved.commit)
    );
    assert_eq!(crate::native_jj::resolve(repo.path(), "not an id"), None);
}

#[test]
fn git_untouched_fork_over_dirty_parent_reports_zero_zero() {
    let repo = git_repo();
    std::fs::write(repo.path().join("DIRT.txt"), "dirt\n").unwrap();
    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(&minted.base.commit)),
        Some((0, 0))
    );
}

#[test]
fn adopted_child_with_uncommitted_work_reports_zero_ahead() {
    let repo = git_repo();
    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    std::fs::write(minted.path.join("ADOPTED.txt"), "adopted\n").unwrap();
    std::fs::write(minted.path.join("SEED.txt"), "edited seed\n").unwrap();
    let candidate = SystemVcs
        .seal(&minted.path, &digest_text("fixture"), "cand-1")
        .unwrap();
    SystemVcs
        .adopt(
            repo.path(),
            &minted.path,
            &minted.base.commit,
            None,
            &candidate,
            None,
        )
        .unwrap();
    let base = candidate.commit.as_str();
    let status = git_in(&minted.path, &["status", "--porcelain"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("SEED.txt"));
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(base)),
        Some((0, 0))
    );
    git_in(&minted.path, &["add", "ADOPTED.txt", "SEED.txt"]);
    git_in(
        &minted.path,
        &["commit", "-qm", "commit exactly the adopted work"],
    );
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(base)),
        Some((0, 0))
    );
    std::fs::write(minted.path.join("ADOPTED.txt"), "edited\n").unwrap();
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(base)),
        Some((1, 0))
    );
    git_in(&minted.path, &["add", "ADOPTED.txt", "SEED.txt"]);
    git_in(&minted.path, &["commit", "-qm", "continued work"]);
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(base)),
        Some((1, 0))
    );
}

#[test]
fn child_uncommitted_edit_counts_one_ahead() {
    let repo = git_repo();
    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    std::fs::write(minted.path.join("AHEAD.txt"), "ahead\n").unwrap();
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(&minted.base.commit)),
        Some((1, 0))
    );
}

#[test]
fn parent_commit_of_mint_dirt_adds_no_behind() {
    let repo = git_repo();
    std::fs::write(repo.path().join("DIRT.txt"), "dirt\n").unwrap();
    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    git_in(repo.path(), &["add", "DIRT.txt"]);
    git_in(repo.path(), &["commit", "-q", "-m", "mint dirt"]);
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(&minted.base.commit)),
        Some((0, 0))
    );
    std::fs::write(repo.path().join("DIRT.txt"), "later\n").unwrap();
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(&minted.base.commit)),
        Some((0, 1))
    );
}

#[test]
fn parent_commit_plus_mint_dirt_counts_one_behind() {
    let repo = git_repo();
    std::fs::write(repo.path().join("a"), "mint dirt\n").unwrap();
    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    std::fs::write(repo.path().join("y"), "committed later\n").unwrap();
    git_in(repo.path(), &["add", "y"]);
    git_in(repo.path(), &["commit", "-qm", "new file"]);
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(&minted.base.commit)),
        Some((0, 1))
    );
}

#[test]
fn parent_behind_cache_is_scoped_to_one_probe_and_keyed_by_base() {
    let repo = git_repo();
    let first = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    let second = SystemVcs
        .mint_from(
            repo.path(),
            "c2",
            "c2",
            repo.path(),
            WorkspaceMode::Fork,
            Some(&first.base),
        )
        .unwrap();
    let mut cache = ParentBehindCache::new();
    assert_eq!(
        SystemVcs.ahead_behind_cached(
            &first.path,
            repo.path(),
            Some(&first.base.commit),
            &mut cache
        ),
        Some((0, 0))
    );
    std::fs::write(repo.path().join("later"), "later\n").unwrap();
    assert_eq!(
        SystemVcs.ahead_behind_cached(
            &second.path,
            repo.path(),
            Some(&first.base.commit),
            &mut cache
        ),
        Some((0, 0))
    );
    assert_eq!(cache.len(), 1);
    let changed_base = SystemVcs
        .seal(repo.path(), &digest_text("cache"), "parent")
        .unwrap();
    SystemVcs.ahead_behind_cached(
        &second.path,
        repo.path(),
        Some(&changed_base.commit),
        &mut cache,
    );
    assert_eq!(cache.len(), 2);
    assert_eq!(
        SystemVcs.ahead_behind_cached(
            &second.path,
            repo.path(),
            Some(&first.base.commit),
            &mut ParentBehindCache::new()
        ),
        Some((0, 1))
    );
}

#[test]
fn secret_only_changes_count_nothing() {
    let repo = git_repo();
    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    std::fs::write(minted.path.join(".env"), "CHILD=1\n").unwrap();
    std::fs::write(repo.path().join(".env"), "PARENT=1\n").unwrap();
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(&minted.base.commit)),
        Some((0, 0))
    );
    assert_eq!(ahead_behind(&minted.path, repo.path(), None), None);
}

#[test]
fn git_only_build_snapshots_colocated_repository_and_nested_paths() {
    if cfg!(feature = "jj-lib") || !cfg!(feature = "git-cli") {
        return;
    }
    let root = git_repo();
    std::fs::create_dir(root.path().join(".jj")).unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    assert_eq!(detect_parent_vcs(&nested).unwrap(), SessionVcs::Git);
    assert!(snapshot(root.path(), "colocated-git").is_ok());
}

#[test]
fn detect_vcs_prefers_jj_when_both_exist() {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::create_dir(root.path().join(".jj")).unwrap();
    if cfg!(feature = "jj-lib") {
        assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Jj);
    } else if cfg!(feature = "git-cli") {
        assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Git);
    } else {
        assert!(detect_vcs(root.path()).is_err());
    }
}

#[test]
fn detect_vcs_git_only() {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    if cfg!(feature = "git-cli") {
        assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Git);
    } else {
        assert_eq!(
            detect_vcs(root.path()).unwrap_err().kind,
            VcsErrorKind::MintUnavailable
        );
    }
}

#[test]
fn feature_selection_refuses_missing_backends_without_fallback() {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    if !cfg!(feature = "git-cli") {
        let error = snapshot(root.path(), "disabled-git").unwrap_err();
        assert_eq!(error.kind, VcsErrorKind::MintUnavailable);
        assert!(error.detail.contains("Git CLI backend is not included"));
    }
    std::fs::create_dir(root.path().join(".jj")).unwrap();
    if cfg!(feature = "jj-lib") {
        assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Jj);
    } else if cfg!(feature = "git-cli") {
        assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Git);
    } else {
        assert!(detect_vcs(root.path()).is_err());
    }
    std::fs::remove_dir(root.path().join(".git")).unwrap();
    if !cfg!(feature = "jj-lib") {
        let error = snapshot(root.path(), "disabled-jj").unwrap_err();
        assert_eq!(error.kind, VcsErrorKind::MintUnavailable);
        assert!(error.detail.contains("this build has no jj support"));
    }
}

#[test]
fn detect_vcs_neither_is_unavailable() {
    let root = tempdir().unwrap();
    assert_eq!(
        detect_vcs(root.path()).unwrap_err().kind,
        VcsErrorKind::MintUnavailable
    );
}

#[test]
fn failed_attempt_snapshot_ref_is_create_only() {
    assert_failed_snapshot_ref_is_create_only(&git_repo());
}

#[test]
fn failed_attempt_snapshot_ref_is_create_only_in_sha256_repositories() {
    assert_failed_snapshot_ref_is_create_only(&git_repo_with_init(&[
        "init",
        "-b",
        "main",
        "--object-format=sha256",
    ]));
}

fn assert_failed_snapshot_ref_is_create_only(repo: &tempfile::TempDir) {
    let namespace = digest_text("create-only-failure");
    let id = "failed-worker-a1";
    std::fs::write(repo.path().join("SEED.txt"), "first failure\n").unwrap();
    let first = SystemVcs.seal(repo.path(), &namespace, id).unwrap();
    let repeated = SystemVcs.seal(repo.path(), &namespace, id).unwrap_err();
    let reference = candidate_ref(&namespace, id);
    assert!(repeated.detail.contains(&reference), "{repeated:?}");
    std::fs::write(repo.path().join("SEED.txt"), "later work\n").unwrap();
    let error = SystemVcs.seal(repo.path(), &namespace, id).unwrap_err();
    assert!(error.detail.contains(&reference), "{error:?}");
    assert_eq!(
        git_text(repo.path(), &["rev-parse", &reference], "ref").unwrap(),
        first.commit
    );
    assert_eq!(
        git_text(
            repo.path(),
            &["show", &format!("{reference}:SEED.txt")],
            "seed"
        )
        .unwrap(),
        "first failure"
    );
}

#[cfg(feature = "jj-lib")]
#[test]
fn failed_attempt_jj_snapshot_ref_is_create_only() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo_with_init_args(&["git", "init", "--no-colocate"]);
    let namespace = digest_text("create-only-jj-failure");
    let id = "failed-worker-a1";
    std::fs::write(repo.path().join("SEED.txt"), "first failure\n").unwrap();
    let first = SystemVcs.seal(repo.path(), &namespace, id).unwrap();
    let repeated = SystemVcs.seal(repo.path(), &namespace, id).unwrap_err();
    let reference = candidate_ref(&namespace, id);
    assert!(repeated.detail.contains(&reference), "{repeated:?}");
    std::fs::write(repo.path().join("SEED.txt"), "later work\n").unwrap();
    let error = SystemVcs.seal(repo.path(), &namespace, id).unwrap_err();
    assert!(error.detail.contains(&reference), "{error:?}");
    let output = Command::new("git")
        .arg("--git-dir=.jj/repo/store/git")
        .args(["rev-parse", &reference])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        first.commit
    );
}

/// the snapshot sees modified, untracked, and deleted paths,
/// and leaves the real index and the working copy alone.
#[test]
fn failed_attempt_snapshot_pins_dirty_files_without_moving_branch() {
    let repo = git_repo();
    let head = git_text(repo.path(), &["rev-parse", "HEAD"], "head").unwrap();
    let branch = git_text(repo.path(), &["symbolic-ref", "HEAD"], "branch").unwrap();
    std::fs::write(repo.path().join("SEED.txt"), "partial work\n").unwrap();
    std::fs::write(repo.path().join("NEW.txt"), "untracked work\n").unwrap();
    std::fs::write(repo.path().join(".env"), "SANITIZED_FIXTURE=1\n").unwrap();
    let index = std::fs::read(repo.path().join(".git/index")).unwrap();
    let namespace = digest_text("failure-test");
    let snap = SystemVcs
        .seal(repo.path(), &namespace, "failed-worker-a1")
        .unwrap();
    let reference = candidate_ref(&namespace, "failed-worker-a1");
    let files = git_text(
        repo.path(),
        &["ls-tree", "-r", "--name-only", &reference],
        "files",
    )
    .unwrap();
    assert!(!files.contains(".env"));
    assert_eq!(
        git_text(repo.path(), &["rev-parse", &reference], "ref").unwrap(),
        snap.commit
    );
    assert_eq!(
        git_text(
            repo.path(),
            &["show", &format!("{reference}:NEW.txt")],
            "new"
        )
        .unwrap(),
        "untracked work"
    );
    assert_eq!(
        git_text(
            repo.path(),
            &["show", &format!("{reference}:SEED.txt")],
            "seed"
        )
        .unwrap(),
        "partial work"
    );
    assert_eq!(
        git_text(repo.path(), &["rev-parse", "HEAD"], "head").unwrap(),
        head
    );
    assert_eq!(
        git_text(repo.path(), &["symbolic-ref", "HEAD"], "branch").unwrap(),
        branch
    );
    assert_eq!(
        std::fs::read(repo.path().join(".git/index")).unwrap(),
        index
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("NEW.txt")).unwrap(),
        "untracked work\n"
    );
}

#[test]
fn snapshot_captures_dirty_state_without_touching_the_index() {
    let repo = git_repo();
    std::fs::write(repo.path().join("SEED.txt"), "changed\n").unwrap();
    std::fs::write(repo.path().join("NEW.txt"), "new\n").unwrap();
    let before = std::fs::read(repo.path().join(".git/index")).unwrap();
    let snap = snapshot(repo.path(), "t1").unwrap();
    assert_eq!(snap.commit.len(), 40, "{}", snap.commit);
    assert_eq!(snap.content.len(), 40, "{}", snap.content);
    assert!(snap.change_id.is_none());
    let listed = git_text(
        repo.path(),
        &["ls-tree", "--name-only", snap.content.as_str()],
        "ls-tree",
    )
    .unwrap();
    assert!(listed.contains("NEW.txt"), "{listed}");
    assert_eq!(
        std::fs::read(repo.path().join(".git/index")).unwrap(),
        before,
        "the real index was rewritten"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
        "changed\n"
    );
}

/// The snapshot commit must not pin a secret the tool policy excludes.
#[test]
fn snapshot_excludes_secret_paths() {
    let repo = git_repo();
    std::fs::write(repo.path().join(".env"), "TOKEN=1\n").unwrap();
    std::fs::write(repo.path().join("app.pem"), "key\n").unwrap();
    std::fs::write(repo.path().join("keep.txt"), "keep\n").unwrap();
    let snap = snapshot(repo.path(), "secrets").unwrap();
    let listed = git_text(
        repo.path(),
        &["ls-tree", "-r", "--name-only", snap.content.as_str()],
        "ls-tree",
    )
    .unwrap();
    assert!(!listed.contains(".env"), "{listed}");
    assert!(!listed.contains("app.pem"), "{listed}");
    assert!(listed.contains("keep.txt"), "{listed}");
}

#[test]
fn parse_merge_tree_conflicts_reads_the_info_section() {
    let stdout = "abc123\n100644 aaa 1\tsrc/a.rs\n100644 bbb 2\tsrc/a.rs\n\nCONFLICT (content)\n";
    assert_eq!(
        parse_merge_tree_conflicts(stdout),
        vec!["src/a.rs".to_owned()]
    );
}

#[test]
fn snapshot_token_matches_commit_content_or_pair() {
    let snap = VcsSnapshot {
        history_left_base: false,
        commit: "c".into(),
        content: "t".into(),
        change_id: None,
    };
    assert_eq!(snap.token(), "c:t");
    assert!(snap.matches_expectation("c:t"));
    assert!(snap.matches_expectation("c"));
    assert!(snap.matches_expectation("t"));
    assert!(!snap.matches_expectation("other"));
    assert!(!snap.matches_expectation(""));
}

/// B4: a subprocess refusal quotes one bounded line of stderr.
#[test]
fn stderr_excerpt_is_one_bounded_line() {
    assert_eq!(stderr_excerpt(b""), "(no stderr)");
    assert_eq!(
        stderr_excerpt(b"error: patch failed: a.rs:1\n\nerror: a.rs: patch does not apply\n"),
        "error: patch failed: a.rs:1 | error: a.rs: patch does not apply"
    );
    let long = vec![b'x'; STDERR_EXCERPT_BYTES * 2];
    assert_eq!(stderr_excerpt(&long).len(), STDERR_EXCERPT_BYTES);
}

/// the hook runs in the copy with the template env and
/// reports a non-zero exit with its output.
#[test]
fn run_mint_hook_reports_exit_and_output() {
    let cwd = tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert("PLAK_HOOK_VALUE".to_owned(), "hooked".to_owned());
    run_mint_hook(cwd.path(), "printf %s \"$PLAK_HOOK_VALUE\" > out.txt", &env).unwrap();
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("out.txt")).unwrap(),
        "hooked"
    );
    let error = run_mint_hook(cwd.path(), "echo why >&2; exit 7", &env).unwrap_err();
    assert!(error.starts_with("hook exited 7:"), "{error}");
    assert!(error.contains("why"), "{error}");
}

/// A `git` that stalls is killed at its ceiling and reported as a
/// timeout, and a git that answers keeps its full output; neither may
/// prompt for credentials.
#[test]
fn a_stalled_git_is_killed_at_its_ceiling() {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = tempdir().unwrap();
    let fake = bin.path().join("git");
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor last; do :; done\n[ \"$last\" = slow ] && /bin/sleep 30\necho \"prompt=$GIT_TERMINAL_PROMPT askpass=[$GIT_ASKPASS] $*\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fake_git = |arg: &str| {
        let mut command = git_command(bin.path());
        command.arg(arg).env("PATH", bin.path());
        command
    };
    let started = std::time::Instant::now();
    let error = output_within(fake_git("slow"), std::time::Duration::from_millis(300)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let output = output_within(fake_git("fast"), GIT_QUERY_TIMEOUT).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "prompt=0 askpass=[] -c core.fsmonitor=false -c core.hooksPath=/dev/null fast"
    );
}

/// Plak's git steps inherit no `GIT_*` steering (`GIT_DIR`,
/// `GIT_WORK_TREE`, `GIT_CONFIG_*`, ...) from the process that started it.
#[test]
fn git_steps_drop_inherited_git_environment() {
    let mut command = Command::new("/usr/bin/env");
    command
        .env("GIT_DIR", "/elsewhere")
        .env("GIT_WORK_TREE", "/elsewhere")
        .env("GIT_CONFIG_PARAMETERS", "'core.hookspath'='/tmp'")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_EXTERNAL_DIFF", "/tmp/differ")
        .env("PLAK_KEEP", "kept");
    strip_git_environment(&mut command);
    let output = command.output().unwrap();
    let environment = String::from_utf8_lossy(&output.stdout);
    assert!(environment.lines().any(|line| line == "PLAK_KEEP=kept"));
    assert!(!environment.lines().any(|line| line.starts_with("GIT_")));

    let built = git_command(Path::new("/"));
    let envs: Vec<_> = built.get_envs().collect();
    for name in [
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_CONFIG_COUNT",
        "GIT_SSH_COMMAND",
    ] {
        assert!(
            envs.contains(&(std::ffi::OsStr::new(name), None)),
            "{name} reaches Plak's git"
        );
    }
}

fn git_in(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A script that leaves `marker` behind when anything runs it.
fn tripwire(dir: &Path, name: &str, marker: &Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let script = dir.join(name);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho ran >> '{}'\necho garbage\nexit 1\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script.display().to_string()
}

/// Repository hooks and the fsmonitor hook never run inside a mint, seal
/// or adopt, even when the repository points `core.hooksPath` at failing
/// hooks.
#[test]
fn repository_hooks_never_run_in_plak_git_steps() {
    let repo = git_repo();
    let tools = tempdir().unwrap();
    let marker = tools.path().join("ran");
    let hooks = tools.path().join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    for hook in [
        "post-checkout",
        "pre-commit",
        "post-index-change",
        "reference-transaction",
    ] {
        tripwire(&hooks, hook, &marker);
    }
    let monitor = tripwire(tools.path(), "fsmonitor", &marker);
    git_in(
        repo.path(),
        &["config", "core.hooksPath", &hooks.display().to_string()],
    );
    git_in(repo.path(), &["config", "core.fsmonitor", &monitor]);

    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    std::fs::write(minted.path.join("SEED.txt"), "child\n").unwrap();
    let candidate = SystemVcs
        .seal(&minted.path, &digest_text("fixture"), "cand-1")
        .unwrap();
    SystemVcs
        .adopt(
            repo.path(),
            &minted.path,
            &minted.base.commit,
            None,
            &candidate,
            None,
        )
        .unwrap();
    SystemVcs.unmint(repo.path(), &minted.path).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
        "child\n"
    );
    assert!(!marker.exists(), "a repository hook ran inside Plak's git");
}

/// External diff drivers and textconv filters configured by the
/// repository are ignored by the adopt diff, so the landed bytes are the
/// candidate's own and no configured program runs.
#[test]
fn adopt_ignores_external_diff_and_textconv() {
    let repo = git_repo();
    let tools = tempdir().unwrap();
    let marker = tools.path().join("ran");
    let external = tripwire(tools.path(), "external", &marker);
    let textconv = tripwire(tools.path(), "textconv", &marker);
    git_in(repo.path(), &["config", "diff.external", &external]);
    git_in(repo.path(), &["config", "diff.evil.textconv", &textconv]);
    std::fs::write(repo.path().join(".gitattributes"), "*.txt diff=evil\n").unwrap();
    git_in(repo.path(), &["add", ".gitattributes"]);
    git_in(repo.path(), &["commit", "-q", "-m", "attributes"]);

    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    std::fs::write(minted.path.join("SEED.txt"), "seed\nchild line\n").unwrap();
    std::fs::write(minted.path.join("NEW.txt"), "new\n").unwrap();
    let candidate = SystemVcs
        .seal(&minted.path, &digest_text("fixture"), "cand-1")
        .unwrap();
    let landing = SystemVcs
        .adopt(
            repo.path(),
            &minted.path,
            &minted.base.commit,
            None,
            &candidate,
            None,
        )
        .unwrap();
    assert_eq!(landing.files_changed, 2);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
        "seed\nchild line\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("NEW.txt")).unwrap(),
        "new\n"
    );
    assert!(
        !marker.exists(),
        "a configured diff program ran inside the adopt"
    );
}

/// The receipt names each landed path with its status, and stops at 64
/// entries while `files_changed` keeps the full count.
#[test]
fn adopt_receipt_lists_changed_files_with_status_and_bounds_at_64() {
    let repo = git_repo();
    for (name, body) in [
        ("0-keep.txt", "keep\n"),
        ("1-gone.txt", "gone\n"),
        ("2-old.txt", "same body\n"),
        ("3-mode.txt", "mode\n"),
    ] {
        std::fs::write(repo.path().join(name), body).unwrap();
    }
    git_in(repo.path(), &["add", "."]);
    git_in(repo.path(), &["commit", "-q", "-m", "base files"]);

    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    std::fs::write(minted.path.join("0-keep.txt"), "keep\nchanged\n").unwrap();
    std::fs::remove_file(minted.path.join("1-gone.txt")).unwrap();
    std::fs::rename(minted.path.join("2-old.txt"), minted.path.join("2-new.txt")).unwrap();
    std::fs::remove_file(minted.path.join("3-mode.txt")).unwrap();
    std::os::unix::fs::symlink("0-keep.txt", minted.path.join("3-mode.txt")).unwrap();
    std::fs::write(minted.path.join("4-added.txt"), "added\n").unwrap();
    for index in 0..70 {
        std::fs::write(
            minted.path.join(format!("9-extra-{index:02}")),
            format!("extra {index}\n"),
        )
        .unwrap();
    }

    let candidate = SystemVcs
        .seal(&minted.path, &digest_text("fixture"), "cand-1")
        .unwrap();
    let landing = SystemVcs
        .adopt(
            repo.path(),
            &minted.path,
            &minted.base.commit,
            None,
            &candidate,
            None,
        )
        .unwrap();
    let (files, truncated) = landing.receipt_files();
    assert_eq!(landing.files_changed, 75, "{files:?}");
    assert!(truncated);
    assert_eq!(files.len(), ADOPT_RECEIPT_FILE_LIMIT);
    for line in [
        "M 0-keep.txt",
        "D 1-gone.txt",
        "R 2-old.txt -> 2-new.txt",
        "T 3-mode.txt",
        "A 4-added.txt",
    ] {
        assert!(
            files.iter().any(|got| got == line),
            "missing {line} in {files:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(repo.path().join("0-keep.txt")).unwrap(),
        "keep\nchanged\n"
    );
    assert!(!repo.path().join("1-gone.txt").exists());
    assert_eq!(
        std::fs::read_to_string(repo.path().join("2-new.txt")).unwrap(),
        "same body\n"
    );
    assert!(!repo.path().join("2-old.txt").exists());
    assert!(repo.path().join("4-added.txt").is_file());
}

#[test]
fn adopt_receipt_uses_lossy_names_without_losing_non_utf8_files() {
    use std::os::unix::ffi::OsStrExt as _;

    let repo = git_repo();
    let minted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    let name = std::ffi::OsStr::from_bytes(b"non-utf8-\xff.txt");
    std::fs::write(minted.path.join(name), "work\n").unwrap();
    let candidate = SystemVcs
        .seal(&minted.path, &digest_text("fixture"), "cand-1")
        .unwrap();
    let landing = SystemVcs
        .adopt(
            repo.path(),
            &minted.path,
            &minted.base.commit,
            None,
            &candidate,
            None,
        )
        .unwrap();
    assert_eq!(
        landing.receipt_files(),
        (vec!["A non-utf8-\u{fffd}.txt".into()], false)
    );
    assert_eq!(std::fs::read(repo.path().join(name)).unwrap(), b"work\n");
}

/// A copy whose directory and registration are already gone (an earlier
/// removal, or by hand) unmints cleanly: only its branch is left to drop,
/// and a repeat is a no-op. A directory that is still present and
/// registered is removed as before.
#[test]
fn unmint_of_an_already_removed_copy_drops_only_its_branch() {
    let repo = git_repo();
    let copy = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "t-c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    let removed = git_in(
        repo.path(),
        &[
            "worktree",
            "remove",
            "--force",
            copy.path.display().to_string().as_str(),
        ],
    );
    assert!(removed.status.success());
    assert!(!copy.path.exists());
    let branch = |repo: &Path| {
        git_run(
            repo,
            &["show-ref", "--verify", "--quiet", "refs/heads/plak/t-c1"],
        )
        .unwrap()
        .status
        .success()
    };
    assert!(branch(repo.path()), "the branch outlives a by-hand removal");

    SystemVcs.unmint(repo.path(), &copy.path).unwrap();
    assert!(!branch(repo.path()), "unmint dropped the leftover branch");
    SystemVcs.unmint(repo.path(), &copy.path).unwrap();

    let live = SystemVcs
        .mint_from(
            repo.path(),
            "c2",
            "t-c2",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    SystemVcs.unmint(repo.path(), &live.path).unwrap();
    assert!(!live.path.exists());
    let listed = String::from_utf8(git_in(repo.path(), &["worktree", "list"]).stdout).unwrap();
    assert!(!listed.contains("t-c2"), "{listed}");
}

/// A crash between `git worktree add` and the tree recording the copy
/// leaves a registered worktree and its `plak/<copy_id>` branch; a bare
/// directory is left by a crash before registration. The next mint of
/// that copy id reclaims either instead of refusing.
#[test]
fn the_next_mint_reclaims_an_orphaned_copy() {
    let repo = git_repo();
    let orphan = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "t-c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    std::fs::write(orphan.path.join("STALE.txt"), "never recorded\n").unwrap();
    let reminted = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "t-c1",
            repo.path(),
            WorkspaceMode::Snapshot,
            None,
        )
        .unwrap();
    assert_eq!(reminted.path, orphan.path);
    assert!(!reminted.path.join("STALE.txt").exists());
    assert!(
        !git_run(
            repo.path(),
            &["show-ref", "--verify", "--quiet", "refs/heads/plak/t-c1"]
        )
        .unwrap()
        .status
        .success(),
        "the orphan's branch survived"
    );
    let listed = String::from_utf8(git_in(repo.path(), &["worktree", "list"]).stdout).unwrap();
    assert_eq!(listed.matches("t-c1").count(), 1, "{listed}");

    // A directory with no registration at all.
    SystemVcs.unmint(repo.path(), &reminted.path).unwrap();
    std::fs::create_dir_all(reminted.path.join("partial")).unwrap();
    let fresh = SystemVcs
        .mint_from(
            repo.path(),
            "c1",
            "t-c1",
            repo.path(),
            WorkspaceMode::Fork,
            None,
        )
        .unwrap();
    assert!(!fresh.path.join("partial").exists());
    assert!(fresh.path.join("SEED.txt").is_file());
}

/// Patches, snapshot indexes and captured git output never land in a
/// shared, guessable temp path: each step gets its own 0700 directory and
/// 0600 files, and they are gone when the step returns.
#[test]
fn scratch_files_are_private_and_removed() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = private_scratch().unwrap();
    let dir_mode = std::fs::metadata(scratch.path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700);
    let file = scratch.path().join("patch");
    drop(create_private(&file).unwrap());
    let file_mode = std::fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(file_mode & 0o777, 0o600);
    assert!(
        create_private(&file).is_err(),
        "an existing file is never reused"
    );
    let kept = scratch.path().to_path_buf();
    drop(scratch);
    assert!(!kept.exists());
}

/// the hook is a shell, and a shell's children outlive
/// `kill(sh)`. A timed-out `on_mint` that had started `cargo build` left
/// it running on the copy's target lock, so the timeout kills the whole
/// process group.
#[test]
fn a_timed_out_mint_hook_kills_the_whole_process_group() {
    let cwd = tempdir().unwrap();
    let env = BTreeMap::new();
    let error = run_mint_hook_bounded(
        cwd.path(),
        "sh -c 'sleep 100 & echo $! > child.pid; wait'",
        &env,
        std::time::Duration::from_millis(300),
    )
    .unwrap_err();
    assert!(error.contains("was killed"), "{error}");
    let pid = std::fs::read_to_string(cwd.path().join("child.pid"))
        .unwrap()
        .trim()
        .to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let alive = Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {pid}"))
            .status()
            .unwrap()
            .success();
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the hook's grandchild {pid} outlived the timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// the hook starts from a cleared environment — PATH, HOME
/// and the supplied hook map, nothing else — so an exported
/// `*_API_KEY` never reaches a template-authored shell command.
#[test]
fn a_mint_hook_does_not_inherit_the_hosts_environment() {
    let cwd = tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert("PLAK_HOOK_VALUE".to_owned(), "hooked".to_owned());
    env.insert("PLAK_LAUNCH_CHECKOUT".to_owned(), "/launch".to_owned());
    env.insert("PLAK_PARENT_PATH".to_owned(), "/parent-copy".to_owned());
    env.insert("PLAK_COPY_ID".to_owned(), "tree-c2".to_owned());
    run_mint_hook(cwd.path(), "env > out.txt", &env).unwrap();
    let seen = std::fs::read_to_string(cwd.path().join("out.txt")).unwrap();
    let names: Vec<&str> = seen
        .lines()
        .filter_map(|line| line.split('=').next())
        .filter(|name| !name.is_empty())
        .collect();
    for name in &names {
        assert!(
            // `sh` sets the last three itself.
            matches!(
                *name,
                "PATH"
                    | "HOME"
                    | "PLAK_HOOK_VALUE"
                    | "PLAK_LAUNCH_CHECKOUT"
                    | "PLAK_PARENT_PATH"
                    | "PLAK_COPY_ID"
                    | "PWD"
                    | "SHLVL"
                    | "_"
            ),
            "the hook inherited {name} from the host: {seen}"
        );
    }
    assert!(names.contains(&"PATH"), "{seen}");
    assert!(names.contains(&"PLAK_HOOK_VALUE"), "{seen}");
    for (name, value) in &env {
        assert!(
            seen.lines().any(|line| line == format!("{name}={value}")),
            "{seen}"
        );
    }
    assert!(
        std::env::vars().count() > names.len(),
        "the host env is wider than what the hook saw"
    );
}
