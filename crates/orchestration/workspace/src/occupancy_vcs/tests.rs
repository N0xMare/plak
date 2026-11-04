use std::process::{Command, Output};

use super::*;
use tempfile::tempdir;

fn fixture_source(path: &Path) -> Option<&str> {
    copy_layout_kind(path)?;
    let id = path.file_name()?.to_str()?;
    let (prefix, agent) = id.split_once('-').unwrap_or(("", id));
    Some(if prefix.len() == 8 { agent } else { id })
}

fn fixture_namespace(copy_id: &str) -> String {
    let prefix = copy_id.split('-').next().unwrap();
    if prefix.len() == 64 {
        prefix.to_owned()
    } else {
        digest_text("fixture")
    }
}

fn mint_worktree_from(
    launch: &Path,
    id: &str,
    parent_cwd: &Path,
    mode: WorkspaceMode,
    base: Option<&VcsSnapshot>,
) -> Result<MintedCopy, VcsError> {
    super::mint_worktree_from(&MintRequest {
        launch,
        agent_id: id,

        namespace: &fixture_namespace(id),
        parent_cwd,
        mode,
        base,
    })
}

fn git_repo() -> tempfile::TempDir {
    git_repo_with_init(&["init", "-b", "main"])
}

#[test]
fn tree_prefix_collision_refuses_mint_without_changing_existing_artifacts() {
    let repo = git_repo();
    let first = format!("1234abcd{}", "0".repeat(56));
    let other = format!("1234abcd{}", "1".repeat(56));
    let request = MintRequest {
        launch: repo.path(),
        agent_id: "c1",
        namespace: &first,
        parent_cwd: repo.path(),
        mode: WorkspaceMode::Fork,
        base: None,
    };
    let copy = SystemVcs.mint_from(&request).unwrap();
    assert_eq!(copy.branch.as_deref(), Some("plak/1234abcd/c1"));
    assert_eq!(copy.path.file_name().unwrap(), "1234abcd-c1");
    let before = git_text(repo.path(), &["for-each-ref"], "refs").unwrap();
    let error = SystemVcs
        .mint_from(&MintRequest {
            namespace: &other,
            agent_id: "c2",
            ..request
        })
        .unwrap_err();
    assert_eq!(error.kind, VcsErrorKind::MintUnavailable);
    assert!(error.detail.contains("tree prefix collision"), "{error:?}");
    assert!(!repo.path().join(".plak/worktrees/1234abcd-c2").exists());
    assert_eq!(
        before,
        git_text(repo.path(), &["for-each-ref"], "refs").unwrap()
    );
    let evidence = pins::common(repo.path()).unwrap().join("plak-namespaces");
    assert_eq!(
        std::fs::read_to_string(evidence.join("1234abcd")).unwrap(),
        first
    );
    assert!(!evidence.join(&other[..16]).exists());
}

#[test]
fn prefix_evidence_lives_until_both_branches_and_pins_are_gone() {
    let repo = git_repo();
    let digest = digest_text("prefix-evidence");
    let common = pins::common(repo.path()).unwrap();
    drop(reserve_copy_namespace(&common, &digest).unwrap());
    let names = copy_names(&digest[..8], "c1");
    assert!(
        git_run(repo.path(), &["branch", &names.branch])
            .unwrap()
            .status
            .success()
    );
    let pin = copy_base_ref(&digest, "c1");
    assert!(
        git_run(repo.path(), &["update-ref", &pin, "HEAD"])
            .unwrap()
            .status
            .success()
    );
    namespace::cleanup(&common, &digest[..16]).unwrap();
    let evidence = common.join("plak-namespaces");
    assert!(evidence.join(&digest[..8]).exists());
    assert!(
        git_run(repo.path(), &["update-ref", "-d", &pin])
            .unwrap()
            .status
            .success()
    );
    namespace::cleanup(&common, &digest[..16]).unwrap();
    assert!(
        evidence.join(&digest[..8]).exists(),
        "branch still owns the prefix"
    );
    assert!(
        git_run(repo.path(), &["branch", "-D", &names.branch])
            .unwrap()
            .status
            .success()
    );
    namespace::cleanup(&common, &digest[..16]).unwrap();
    assert!(!evidence.join(&digest[..8]).exists());
    assert!(!evidence.join(&digest[..16]).exists());

    // Both a cut before the short claim's publication and a cut after its
    // deletion leave only the long witness. Cleanup must finish either one.
    drop(reserve_copy_namespace(&common, &digest).unwrap());
    std::fs::remove_file(evidence.join(&digest[..8])).unwrap();
    namespace::cleanup(&common, &digest[..16]).unwrap();
    assert!(!evidence.join(&digest[..16]).exists());
}

#[test]
fn adoption_pin_failure_after_apply_requires_explicit_recovery() {
    assert_adoption_pin_failure(false);
}

#[test]
fn no_op_adoption_pin_failure_is_unavailable() {
    assert_adoption_pin_failure(true);
}

fn assert_adoption_pin_failure(already_present: bool) {
    let repo = git_repo();
    let namespace = digest_text("failed-adoption-pins");
    let copy = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            namespace: &namespace,
            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
        })
        .unwrap();
    std::fs::write(copy.path.join("work"), "retained landing").unwrap();
    let candidate = SystemVcs
        .seal(&copy.path, &namespace, "candidate", &copy.base.commit)
        .unwrap();
    if already_present {
        std::fs::write(repo.path().join("work"), "retained landing").unwrap();
    }
    let refs = candidate_refs(&namespace, "candidate");
    let blocker = format!("{}/blocked", refs[3]);
    assert!(
        git_run(repo.path(), &["update-ref", &blocker, "HEAD"])
            .unwrap()
            .status
            .success()
    );
    let failure = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &copy.path,
            source_agent_id: Some("c1"),
            base_commit: &copy.base.commit,
            base_change_id: None,
            candidate: &candidate,
            expected_target: None,
            namespace: &namespace,
            candidate_id: "candidate",
        })
        .unwrap_err();
    assert!(
        matches!(
            (&failure, already_present),
            (AdoptFailure::Unavailable(_), true) | (AdoptFailure::RecoveryRequired(_), false)
        ),
        "{failure:?}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("work")).unwrap(),
        "retained landing"
    );
    assert!(
        !git_run(repo.path(), &["show-ref", "--verify", "--quiet", &refs[2]])
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        git_text(
            repo.path(),
            &["rev-parse", &copy_base_ref(&namespace, "c1")],
            "unchanged base"
        )
        .unwrap()
        .trim(),
        copy.base.commit
    );
}

#[test]
fn seal_pins_candidate_and_base_atomically() {
    let repo = git_repo();
    let namespace = digest_text("pin-transaction");
    let copy = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            namespace: &namespace,
            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
        })
        .unwrap();
    std::fs::write(copy.path.join("work"), "candidate bytes").unwrap();
    let before = git_text(repo.path(), &["for-each-ref", "refs/plak/"], "refs").unwrap();
    assert!(
        SystemVcs
            .seal(&copy.path, &namespace, "candidate", "not-an-object")
            .is_err()
    );
    assert_eq!(
        git_text(repo.path(), &["for-each-ref", "refs/plak/"], "refs").unwrap(),
        before
    );
    let sealed = SystemVcs
        .seal(&copy.path, &namespace, "candidate", &copy.base.commit)
        .unwrap();
    let refs = candidate_refs(&namespace, "candidate");
    assert_eq!(
        git_text(repo.path(), &["rev-parse", &refs[0]], "candidate")
            .unwrap()
            .trim(),
        sealed.commit
    );
    assert_eq!(
        git_text(repo.path(), &["rev-parse", &refs[1]], "base")
            .unwrap()
            .trim(),
        copy.base.commit
    );
    assert_eq!(
        git_text(
            repo.path(),
            &["rev-parse", &copy_base_ref(&namespace, "c1")],
            "mint base"
        )
        .unwrap()
        .trim(),
        copy.base.commit
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one retention lifecycle must keep its original candidate and adoption pins through prune and GC"
)]
fn adopted_candidate_and_provenance_survive_copy_prune_and_gc_prune_now() {
    let repo = git_repo();
    let namespace = digest_text("retained-provenance");
    let mint = |agent, base| {
        SystemVcs
            .mint_from(&MintRequest {
                launch: repo.path(),
                agent_id: agent,

                namespace: &namespace,
                parent_cwd: repo.path(),
                mode: WorkspaceMode::Fork,
                base,
            })
            .unwrap()
    };
    let first = mint("c1", None);
    std::fs::write(first.path.join("first"), "first work").unwrap();
    let candidate = SystemVcs
        .seal(&first.path, &namespace, "first", &first.base.commit)
        .unwrap();
    let landing = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &first.path,
            source_agent_id: fixture_source(&first.path),
            base_commit: &first.base.commit,
            base_change_id: None,
            candidate: &candidate,
            expected_target: None,
            namespace: &namespace,
            candidate_id: "first",
        })
        .unwrap();
    let refs = candidate_refs(&namespace, "first");
    assert_eq!(
        git_text(
            repo.path(),
            &["rev-parse", &copy_base_ref(&namespace, "c1")],
            "advanced base"
        )
        .unwrap()
        .trim(),
        candidate.commit
    );
    for (reference, expected) in [
        (&refs[2], &landing.target_before),
        (&refs[3], &landing.target_after),
    ] {
        assert_eq!(
            git_text(repo.path(), &["rev-parse", reference], "provenance")
                .unwrap()
                .trim(),
            expected
        );
    }
    assert_eq!(
        git_text(
            repo.path(),
            &["rev-parse", &format!("{}^", landing.target_after)],
            "parent"
        )
        .unwrap()
        .trim(),
        landing.target_before
    );
    let repeated = SystemVcs
        .seal(
            &first.path,
            &namespace,
            "already-present",
            &first.base.commit,
        )
        .unwrap();
    let no_op = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &first.path,
            source_agent_id: Some("c1"),
            base_commit: &first.base.commit,
            base_change_id: None,
            candidate: &repeated,
            expected_target: None,
            namespace: &namespace,
            candidate_id: "already-present",
        })
        .unwrap();
    assert_eq!(no_op.files_changed, 0);
    assert_eq!(
        git_text(
            repo.path(),
            &["rev-parse", &format!("{}^", no_op.target_after)],
            "no-op parent"
        )
        .unwrap()
        .trim(),
        no_op.target_before
    );
    assert_eq!(
        git_text(
            repo.path(),
            &["rev-parse", &copy_base_ref(&namespace, "c1")],
            "no-op base"
        )
        .unwrap()
        .trim(),
        repeated.commit
    );
    SystemVcs
        .unmint(repo.path(), &first.path, &namespace)
        .unwrap();
    assert!(
        git_run(repo.path(), &["gc", "--prune=now"])
            .unwrap()
            .status
            .success()
    );
    for reference in &refs {
        assert!(
            git_run(repo.path(), &["cat-file", "-e", reference])
                .unwrap()
                .status
                .success()
        );
    }
    // A new copy pins its input independently of that input's record. Once
    // the old record is collected and all four refs dropped, adoption still
    // has its exact base, even after immediate collection.
    let second = mint("c2", Some(&candidate));
    SystemVcs.unpin(repo.path(), &namespace, "first").unwrap();
    for reference in &refs {
        assert!(
            !git_run(repo.path(), &["show-ref", "--verify", "--quiet", reference])
                .unwrap()
                .status
                .success()
        );
    }
    assert!(
        git_run(repo.path(), &["gc", "--prune=now"])
            .unwrap()
            .status
            .success()
    );
    std::fs::write(second.path.join("second"), "second work").unwrap();
    let next = SystemVcs
        .seal(&second.path, &namespace, "second", &second.base.commit)
        .unwrap();
    SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &second.path,
            source_agent_id: fixture_source(&second.path),
            base_commit: &second.base.commit,
            base_change_id: None,
            candidate: &next,
            expected_target: None,
            namespace: &namespace,
            candidate_id: "second",
        })
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("first")).unwrap(),
        "first work"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("second")).unwrap(),
        "second work"
    );
}

#[test]
fn conflict_parser_preserves_whitespace_and_tabs_in_paths() {
    let path = " leading\ttab and trailing ";
    let info = format!(
        "tree\0{mode} blob 1\t{path}\0{mode} blob 2\t{path}\0\0",
        mode = "100644"
    );
    assert_eq!(
        parse_merge_tree_conflicts(&info),
        vec![ConflictPath {
            path: path.into(),
            kind: "contents".into()
        }]
    );
    let messages = format!("tree\0\x001\0{path}\0CONFLICT (contents)\0message\0");
    assert_eq!(
        parse_merge_tree_conflicts(&messages),
        vec![ConflictPath {
            path: path.into(),
            kind: "contents".into()
        }]
    );
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
            .args([
                "branch",
                &copy_names(&digest_text("fixture")[..8], "c1").branch
            ])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let minted = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    assert_eq!(
        minted.branch,
        Some(copy_names(&digest_text("fixture")[..8], "c1").branch)
    );
    assert!(minted.path.join("SEED.txt").is_file());
}

/// A crashed first tree can retry cleanup after its `c1` was removed and
/// a second tree minted its own `c1`. Tree-scoped copy ids make those
/// physical resources distinct, so the stale retry cannot remove the
/// second tree's worktree or branch.
#[test]
fn stale_tree_cleanup_cannot_remove_another_trees_reused_child_id() {
    let repo = git_repo();
    let first_namespace = digest_text("tree-first");
    let second_namespace = digest_text("tree-second");
    let first = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &first_namespace,
        })
        .unwrap();
    SystemVcs
        .unmint(repo.path(), &first.path, &first_namespace)
        .unwrap();

    let second = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &second_namespace,
        })
        .unwrap();
    assert_ne!(first.path, second.path);
    // A stale repeat of the first tree's cleanup finds its copy already
    // gone and succeeds without touching the second tree's copy.
    SystemVcs
        .unmint(repo.path(), &first.path, &first_namespace)
        .unwrap();
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    std::fs::write(minted.path.join("ADOPTED.txt"), "adopted\n").unwrap();
    std::fs::write(minted.path.join("SEED.txt"), "edited seed\n").unwrap();
    let candidate = SystemVcs
        .seal(
            &minted.path,
            &digest_text("fixture"),
            "cand-1",
            &minted.base.commit,
        )
        .unwrap();
    SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &minted.path,
            source_agent_id: fixture_source(&minted.path),
            base_commit: &minted.base.commit,
            base_change_id: None,
            candidate: &candidate,
            expected_target: None,
            namespace: &digest_text("fixture"),
            candidate_id: "cand-1",
        })
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    let second = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c2",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: Some(&first.base),
            namespace: &fixture_namespace("c2"),
        })
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
        .seal(repo.path(), &digest_text("cache"), "parent", "HEAD")
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    std::fs::write(minted.path.join(".env"), "CHILD=1\n").unwrap();
    std::fs::write(repo.path().join(".env"), "PARENT=1\n").unwrap();
    assert_eq!(
        ahead_behind(&minted.path, repo.path(), Some(&minted.base.commit)),
        Some((0, 0))
    );
    assert_eq!(ahead_behind(&minted.path, repo.path(), None), None);
}

fn colocate_jj(repo: &Path) {
    assert!(
        Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
            .args([
                "--no-pager",
                "--config",
                "user.name=plak",
                "--config",
                "user.email=plak@local",
                "git",
                "init",
                "--colocate",
            ])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(repo)
            .status()
            .unwrap()
            .success(),
        "jj git init --colocate failed"
    );
}

#[test]
fn git_worktree_copy_unmints_through_git_after_colocation() {
    if !jj_available() {
        return;
    }
    let repo = git_repo();
    let minted = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("t-c1"),
        })
        .unwrap();
    colocate_jj(repo.path());
    assert_eq!(detect_vcs(repo.path()).unwrap(), SessionVcs::Jj);
    SystemVcs
        .unmint(repo.path(), &minted.path, &digest_text("fixture"))
        .unwrap();
    assert!(!minted.path.exists());
    assert!(
        !git_run(
            repo.path(),
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", minted.branch.as_ref().unwrap())
            ],
        )
        .unwrap()
        .status
        .success()
    );
}

#[test]
fn git_fork_seals_after_launch_colocation() {
    if !jj_available() {
        return;
    }
    let repo = git_repo();
    let minted = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    std::fs::write(minted.path.join("H.txt"), "child\n").unwrap();
    colocate_jj(repo.path());
    std::fs::create_dir(minted.path.join(".jj")).unwrap();
    let refused =
        maintenance::inspect(repo.path(), std::slice::from_ref(&minted.path), &[]).unwrap_err();
    assert!(
        refused.detail.contains("UNADOPTED COMMITS"),
        "{}",
        refused.detail
    );
    let namespace = digest_text("fixture");
    let sealed = SystemVcs
        .seal(&minted.path, &namespace, "cand-1", &minted.base.commit)
        .unwrap();
    let reference = candidate_ref(&namespace, "cand-1");
    assert!(
        git_run(
            repo.path(),
            &["show-ref", "--verify", "--quiet", reference.as_str()],
        )
        .unwrap()
        .status
        .success(),
        "seal did not pin {reference}"
    );
    let refused = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &minted.path,
            source_agent_id: fixture_source(&minted.path),
            base_commit: &minted.base.commit,
            base_change_id: None,
            candidate: &sealed,
            expected_target: None,
            namespace: &digest_text("fixture"),
            candidate_id: "cand-1",
        })
        .unwrap_err();
    assert!(
        matches!(refused, AdoptFailure::Unavailable(ref reason) if reason.contains("git candidate into a jj target")),
        "{refused:?}"
    );
}

#[test]
fn colocated_launch_keeps_all_copy_queries_and_maintenance_on_git() {
    if !jj_available() {
        return;
    }
    let repo = git_repo();
    std::fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
    git_in(repo.path(), &["add", ".gitignore"]);
    git_in(repo.path(), &["commit", "-qm", "ignore build cache"]);
    let copy = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    std::fs::write(copy.path.join("WIP.txt"), "work\n").unwrap();
    colocate_jj(repo.path());
    let namespace = digest_text("colocation");
    let sealed = SystemVcs
        .seal(&copy.path, &namespace, "candidate", &copy.base.commit)
        .unwrap();
    assert!(SystemVcs.dirty(&copy.path));
    assert_eq!(SystemVcs.label(&copy.path), copy.branch);
    assert_eq!(
        SystemVcs.ahead_behind(&copy.path, repo.path(), Some(&copy.base.commit)),
        Some((1, 0))
    );
    assert!(maintenance::dirty(&copy.path).unwrap());
    std::fs::create_dir(copy.path.join("target")).unwrap();
    std::fs::write(copy.path.join("target/cache"), "disposable\n").unwrap();
    maintenance::check_reclaimable(&copy.path, Some(&sealed)).unwrap();
    assert!(maintenance::trim_disposable_target(&copy.path).unwrap());
    assert!(!copy.path.join("target").exists());
    let refused =
        maintenance::inspect(repo.path(), std::slice::from_ref(&copy.path), &[]).unwrap_err();
    assert!(
        refused.detail.contains("UNADOPTED COMMITS"),
        "{}",
        refused.detail
    );
    let refused = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &copy.path,
            source_agent_id: fixture_source(&copy.path),
            base_commit: &copy.base.commit,
            base_change_id: None,
            candidate: &sealed,
            expected_target: None,
            namespace: &digest_text("fixture"),
            candidate_id: "cand-1",
        })
        .unwrap_err();
    assert!(
        matches!(refused, AdoptFailure::Unavailable(reason) if reason.contains("git candidate into a jj target: not yet supported"))
    );
    SystemVcs
        .unpin(&copy.path, &namespace, "candidate")
        .unwrap();
    assert!(
        !git_run(
            repo.path(),
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &candidate_ref(&namespace, "candidate")
            ]
        )
        .unwrap()
        .status
        .success()
    );
}

#[test]
fn jj_adoption_omits_file_listing_even_when_files_changed_is_nonzero() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let copy = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    std::fs::write(copy.path.join("WORK.txt"), "work\n").unwrap();
    let sealed = SystemVcs
        .seal(
            &copy.path,
            &digest_text("jj-receipt"),
            "candidate",
            &copy.base.commit,
        )
        .unwrap();
    let landing = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &copy.path,
            source_agent_id: fixture_source(&copy.path),
            base_commit: &copy.base.commit,
            base_change_id: copy.base.change_id.as_deref(),
            candidate: &sealed,
            expected_target: None,
            namespace: &digest_text("fixture"),
            candidate_id: "cand-1",
        })
        .unwrap();
    assert_ne!(landing.files_changed, 0);
    assert_eq!(landing.receipt_files(), (Vec::new(), false));
    assert_eq!(
        std::fs::read_to_string(repo.path().join("WORK.txt")).unwrap(),
        "work\n"
    );
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
    let first = SystemVcs.seal(repo.path(), &namespace, id, "HEAD").unwrap();
    let repeated = SystemVcs
        .seal(repo.path(), &namespace, id, "HEAD")
        .unwrap_err();
    let reference = candidate_ref(&namespace, id);
    assert!(repeated.detail.contains(&reference), "{repeated:?}");
    std::fs::write(repo.path().join("SEED.txt"), "later work\n").unwrap();
    let error = SystemVcs
        .seal(repo.path(), &namespace, id, "HEAD")
        .unwrap_err();
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
    let base = jj_snapshot(repo.path()).unwrap().commit;
    std::fs::write(repo.path().join("SEED.txt"), "first failure\n").unwrap();
    let first = SystemVcs.seal(repo.path(), &namespace, id, &base).unwrap();
    let repeated = SystemVcs
        .seal(repo.path(), &namespace, id, &base)
        .unwrap_err();
    let reference = candidate_ref(&namespace, id);
    assert!(repeated.detail.contains(&reference), "{repeated:?}");
    std::fs::write(repo.path().join("SEED.txt"), "later work\n").unwrap();
    let error = SystemVcs
        .seal(repo.path(), &namespace, id, &base)
        .unwrap_err();
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
        .seal(repo.path(), &namespace, "failed-worker-a1", "HEAD")
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
    let stdout = "\
abc123\0\
100644 aaa 1\tsrc/a.rs\0\
100644 bbb 2\timg.png\0\
100644 ccc 1\tgone.txt\0\
\0\
1\0src/a.rs\0Auto-merging\0Auto-merging src/a.rs\n\0\
1\0src/a.rs\0CONFLICT (contents)\0CONFLICT (content): Merge conflict in src/a.rs\n\0\
1\0img.png\0CONFLICT (binary)\0warning: Cannot merge binary files: img.png\n\0\
1\0img.png\0CONFLICT (contents)\0CONFLICT (content): Merge conflict in img.png\n\0\
1\0gone.txt\0CONFLICT (modify/delete)\0CONFLICT (modify/delete): gone.txt deleted\n\0";
    assert_eq!(
        parse_merge_tree_conflicts(stdout),
        vec![
            ConflictPath {
                path: "src/a.rs".to_owned(),
                kind: "contents".to_owned(),
            },
            ConflictPath {
                path: "img.png".to_owned(),
                kind: "binary".to_owned(),
            },
            ConflictPath {
                path: "gone.txt".to_owned(),
                kind: "modify/delete".to_owned(),
            },
        ]
    );
}

/// A real `merge-tree -z` names binary and modify/delete instead of collapsing
/// them into a contents conflict. The child copy stays untouched.
#[test]
fn binary_and_modify_delete_conflicts_name_their_kinds() {
    let repo = git_repo();
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/a.rs"), "base\n").unwrap();
    std::fs::write(repo.path().join("img.png"), b"AAA").unwrap();
    std::fs::write(repo.path().join("gone.txt"), "base\n").unwrap();
    git_in(repo.path(), &["add", "."]);
    git_in(repo.path(), &["commit", "-q", "-m", "files"]);

    let minted = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    std::fs::write(repo.path().join("src/a.rs"), "parent\n").unwrap();
    std::fs::write(repo.path().join("img.png"), b"BBB\0").unwrap();
    std::fs::write(repo.path().join("gone.txt"), "parent\n").unwrap();
    std::fs::write(minted.path.join("src/a.rs"), "child\n").unwrap();
    std::fs::write(minted.path.join("img.png"), b"CCC\0").unwrap();
    std::fs::remove_file(minted.path.join("gone.txt")).unwrap();

    let candidate = SystemVcs
        .seal(
            &minted.path,
            &digest_text("fixture"),
            "cand-1",
            &minted.base.commit,
        )
        .unwrap();
    let failure = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &minted.path,
            source_agent_id: fixture_source(&minted.path),
            base_commit: &minted.base.commit,
            base_change_id: None,
            candidate: &candidate,
            expected_target: None,
            namespace: &digest_text("fixture"),
            candidate_id: "cand-1",
        })
        .unwrap_err();
    let AdoptFailure::Conflict { paths, .. } = failure else {
        panic!("expected a conflict, got {failure:?}");
    };
    let kind = |name: &str| {
        paths
            .iter()
            .find(|conflict| conflict.path == name)
            .map(|conflict| conflict.kind.as_str())
    };
    assert_eq!(kind("src/a.rs"), Some("contents"), "{paths:?}");
    assert_eq!(kind("img.png"), Some("binary"), "{paths:?}");
    assert_eq!(kind("gone.txt"), Some("modify/delete"), "{paths:?}");
    assert_eq!(
        std::fs::read(minted.path.join("src/a.rs")).unwrap(),
        b"child\n"
    );
    assert_eq!(
        std::fs::read(minted.path.join("img.png")).unwrap(),
        b"CCC\0"
    );
    assert!(!minted.path.join("gone.txt").exists());
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    std::fs::write(minted.path.join("SEED.txt"), "child\n").unwrap();
    let candidate = SystemVcs
        .seal(
            &minted.path,
            &digest_text("fixture"),
            "cand-1",
            &minted.base.commit,
        )
        .unwrap();
    SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &minted.path,
            source_agent_id: fixture_source(&minted.path),
            base_commit: &minted.base.commit,
            base_change_id: None,
            candidate: &candidate,
            expected_target: None,
            namespace: &digest_text("fixture"),
            candidate_id: "cand-1",
        })
        .unwrap();
    SystemVcs
        .unmint(repo.path(), &minted.path, &digest_text("fixture"))
        .unwrap();
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    std::fs::write(minted.path.join("SEED.txt"), "seed\nchild line\n").unwrap();
    std::fs::write(minted.path.join("NEW.txt"), "new\n").unwrap();
    let candidate = SystemVcs
        .seal(
            &minted.path,
            &digest_text("fixture"),
            "cand-1",
            &minted.base.commit,
        )
        .unwrap();
    let landing = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &minted.path,
            source_agent_id: fixture_source(&minted.path),
            base_commit: &minted.base.commit,
            base_change_id: None,
            candidate: &candidate,
            expected_target: None,
            namespace: &digest_text("fixture"),
            candidate_id: "cand-1",
        })
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
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
        .seal(
            &minted.path,
            &digest_text("fixture"),
            "cand-1",
            &minted.base.commit,
        )
        .unwrap();
    let landing = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &minted.path,
            source_agent_id: fixture_source(&minted.path),
            base_commit: &minted.base.commit,
            base_change_id: None,
            candidate: &candidate,
            expected_target: None,
            namespace: &digest_text("fixture"),
            candidate_id: "cand-1",
        })
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

// APFS refuses file names that are not valid UTF-8, so the case only exists
// on Linux filesystems.
#[cfg(target_os = "linux")]
#[test]
fn adopt_receipt_uses_lossy_names_without_losing_non_utf8_files() {
    use std::os::unix::ffi::OsStrExt as _;

    let repo = git_repo();
    let minted = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("c1"),
        })
        .unwrap();
    let name = std::ffi::OsStr::from_bytes(b"non-utf8-\xff.txt");
    std::fs::write(minted.path.join(name), "work\n").unwrap();
    let candidate = SystemVcs
        .seal(
            &minted.path,
            &digest_text("fixture"),
            "cand-1",
            &minted.base.commit,
        )
        .unwrap();
    let landing = SystemVcs
        .adopt(&AdoptRequest {
            resolution_refusal: None,
            revert: None,
            accept_conflicts: false,
            conflict_pin: None,
            resolver_paths: &[],
            resolved_source: None,
            target_path: repo.path(),
            child_path: &minted.path,
            source_agent_id: fixture_source(&minted.path),
            base_commit: &minted.base.commit,
            base_change_id: None,
            candidate: &candidate,
            expected_target: None,
            namespace: &digest_text("fixture"),
            candidate_id: "cand-1",
        })
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
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("t-c1"),
        })
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
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", copy.branch.as_ref().unwrap()),
            ],
        )
        .unwrap()
        .status
        .success()
    };
    assert!(branch(repo.path()), "the branch outlives a by-hand removal");

    SystemVcs
        .unmint(repo.path(), &copy.path, &digest_text("fixture"))
        .unwrap();
    assert!(!branch(repo.path()), "unmint dropped the leftover branch");
    SystemVcs
        .unmint(repo.path(), &copy.path, &digest_text("fixture"))
        .unwrap();

    let live = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c2",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("t-c2"),
        })
        .unwrap();
    SystemVcs
        .unmint(repo.path(), &live.path, &digest_text("fixture"))
        .unwrap();
    assert!(!live.path.exists());
    let listed = String::from_utf8(git_in(repo.path(), &["worktree", "list"]).stdout).unwrap();
    assert!(!listed.contains(live.path.to_str().unwrap()), "{listed}");
}

/// A crash between `git worktree add` and the tree recording the copy
/// leaves a registered worktree and its `plak/<copy_id>` branch; a bare
/// directory is left by a crash before registration. The next mint of
/// that copy id reclaims either instead of refusing.
#[test]
fn the_next_mint_reclaims_an_orphaned_copy() {
    let repo = git_repo();
    let orphan = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("t-c1"),
        })
        .unwrap();
    std::fs::write(orphan.path.join("STALE.txt"), "never recorded\n").unwrap();
    let reminted = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Snapshot,
            base: None,
            namespace: &fixture_namespace("t-c1"),
        })
        .unwrap();
    assert_eq!(reminted.path, orphan.path);
    assert!(!reminted.path.join("STALE.txt").exists());
    assert!(
        !git_run(
            repo.path(),
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", orphan.branch.as_ref().unwrap())
            ]
        )
        .unwrap()
        .status
        .success(),
        "the orphan's branch survived"
    );
    let listed = String::from_utf8(git_in(repo.path(), &["worktree", "list"]).stdout).unwrap();
    assert_eq!(
        listed.matches(reminted.path.to_str().unwrap()).count(),
        1,
        "{listed}"
    );

    // A directory with no registration at all.
    SystemVcs
        .unmint(repo.path(), &reminted.path, &digest_text("fixture"))
        .unwrap();
    std::fs::create_dir_all(reminted.path.join("partial")).unwrap();
    let fresh = SystemVcs
        .mint_from(&MintRequest {
            launch: repo.path(),
            agent_id: "c1",

            parent_cwd: repo.path(),
            mode: WorkspaceMode::Fork,
            base: None,
            namespace: &fixture_namespace("t-c1"),
        })
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
    // A liveness bound only: generous, so load never trips it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
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

/// The production port never reads an uninspectable path as absent: a path
/// behind a directory it may not search, and a copy root it may not read, are
/// errors. Only a copy root that does not exist lists nothing.
#[test]
fn system_presence_questions_answer_an_uninspectable_path_with_an_error() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempdir().unwrap();
    let launch = root.path().join("launch");
    std::fs::create_dir_all(&launch).unwrap();
    assert!(SystemVcs.jj_copy_dirs(&launch).unwrap().is_empty());

    let copies = launch.join(".plak/workspaces");
    std::fs::create_dir_all(copies.join("c1")).unwrap();
    assert_eq!(
        SystemVcs.jj_copy_dirs(&launch).unwrap(),
        std::collections::BTreeSet::from(["c1".to_owned()])
    );
    assert!(SystemVcs.path_exists(&copies.join("c1")).unwrap());
    assert!(!SystemVcs.path_exists(&copies.join("c2")).unwrap());

    std::fs::set_permissions(&copies, std::fs::Permissions::from_mode(0o000)).unwrap();
    let searchable = std::fs::read_dir(&copies).is_ok();
    let listed = SystemVcs.jj_copy_dirs(&launch);
    let present = SystemVcs.path_exists(&copies.join("c1"));
    std::fs::set_permissions(&copies, std::fs::Permissions::from_mode(0o700)).unwrap();
    if searchable {
        // A process that ignores permissions cannot observe the refusal.
        return;
    }
    assert!(listed.is_err(), "an unreadable copy root listed {listed:?}");
    assert!(
        present.is_err(),
        "an unsearchable parent answered {present:?}"
    );
}
