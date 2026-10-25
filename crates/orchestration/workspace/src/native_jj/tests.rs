use super::{adopt, mint, snapshot};
use crate::vcs_fixtures::{jj_available, jj_in, jj_repo};
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo;
#[test]
fn jj_counts_exclude_empty_commits_and_the_copied_mint_tree() {
    if !jj_available() {
        return;
    }
    let root = jj_repo();
    let clean = mint(root.path(), "clean-count", root.path(), None).unwrap();
    assert_eq!(
        super::ahead_behind(&clean.path, root.path(), Some(&clean.base.commit)),
        Some((0, 0))
    );
    std::fs::write(root.path().join("dirty.txt"), "at mint").unwrap();
    let child = mint(root.path(), "dirty-count", root.path(), None).unwrap();
    assert_eq!(
        super::ahead_behind(&child.path, root.path(), Some(&child.base.commit)),
        Some((0, 0))
    );
    std::fs::write(root.path().join("dirty.txt"), "after mint").unwrap();
    snapshot(root.path()).unwrap();
    assert_eq!(
        super::ahead_behind(&child.path, root.path(), Some(&child.base.commit)),
        Some((0, 1))
    );
    for n in 1..=3 {
        std::fs::write(child.path.join(format!("child-{n}.txt")), "child").unwrap();
        jj_in(&child.path, &["commit", "-m", "child work"]);
        assert_eq!(
            super::ahead_behind(&child.path, root.path(), Some(&child.base.commit)),
            Some((n, 1))
        );
    }
}

#[test]
fn upgraded_jj_copy_requires_a_matching_base_pin_and_a_recorded_row() {
    let root = native_repo();
    let child = mint(root.path(), "base-validation", root.path(), None).unwrap();
    assert!(super::validate_copy_base(&child.path, &child.base.commit).is_ok());
    let head = snapshot(&child.path).unwrap();
    assert!(
        super::validate_copy_base(&child.path, &head.commit)
            .unwrap_err()
            .detail
            .contains("jj copy minted by an older build")
    );
    let missing = crate::git_redirect_fence::install_for_copy(&child.path, None).unwrap_err();
    assert!(missing.contains("no recorded tree row"));
    assert!(
        super::validate_copy_base(&child.path, &snapshot(root.path()).unwrap().commit).is_err()
    );
    super::pin_base(&child.path, "base-validation", None).unwrap();
    assert!(
        super::validate_copy_base(&child.path, &child.base.commit)
            .unwrap_err()
            .detail
            .contains("jj copy minted by an older build")
    );
}

#[test]
fn stale_base_pin_is_replaced_and_unused_frozen_base_is_abandoned() {
    let root = native_repo();
    let stale = snapshot(root.path()).unwrap();
    super::pin_base(root.path(), "reused-pin", Some(&stale.commit)).unwrap();
    let child = mint(root.path(), "reused-pin", root.path(), None).unwrap();
    assert_ne!(child.base.commit, stale.commit);
    super::forget(root.path(), "reused-pin").unwrap();
    pollster::block_on(async {
        let ws = super::load(root.path()).unwrap();
        let repo = ws.repo_loader().load_at_head().await.unwrap();
        assert!(super::pinned_base(&repo, "reused-pin").is_none());
        assert!(
            !repo
                .view()
                .heads()
                .contains(&jj_lib::backend::CommitId::try_from_hex(&child.base.commit).unwrap())
        );
    });
}

#[test]
fn git_discovery_ceiling_is_honored_by_environment_aware_gix() {
    if std::env::var_os("PLAK_TEST_GIX_CEILING_PROBE").is_some() {
        assert!(
            gix::ThreadSafeRepository::discover_with_environment_overrides_opts(
                ".",
                gix::discover::upwards::Options::default(),
                gix::sec::trust::Mapping::default(),
            )
            .is_err()
        );
        return;
    }
    let launch = crate::vcs_fixtures::git_repo();
    let copy = launch.path().join("copies/child");
    std::fs::create_dir_all(copy.join("sub")).unwrap();
    assert!(gix::discover(&copy).is_ok());
    assert!(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_jj::tests::git_discovery_ceiling_is_honored_by_environment_aware_gix"
            ])
            .current_dir(copy.join("sub"))
            .env_clear()
            .env("GIT_CEILING_DIRECTORIES", copy.parent().unwrap())
            .env("PLAK_TEST_GIX_CEILING_PROBE", "1")
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn running_forks_survive_parent_edit_and_snapshot() {
    let repo = native_repo();
    let first = mint(repo.path(), "first", repo.path(), None).unwrap();
    let second = mint(repo.path(), "second", repo.path(), None).unwrap();
    std::fs::write(first.path.join("SEED.txt"), "first work").unwrap();
    std::fs::write(second.path.join("new.txt"), "second work").unwrap();
    std::fs::write(repo.path().join("SEED.txt"), "parent work").unwrap();
    snapshot(repo.path()).unwrap();
    for child in [&first, &second] {
        assert_ne!(snapshot(&child.path).unwrap().content, child.base.content);
        assert_eq!(
            super::resolve(repo.path(), &child.base.commit).as_deref(),
            Some(child.base.commit.as_str())
        );
    }
    assert_eq!(
        std::fs::read_to_string(first.path.join("SEED.txt")).unwrap(),
        "first work"
    );
    assert_eq!(
        std::fs::read_to_string(second.path.join("new.txt")).unwrap(),
        "second work"
    );
}

#[test]
fn owner_describe_commit_and_new_do_not_stale_forks() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let child = mint(repo.path(), "owner-commands", repo.path(), None).unwrap();
    std::fs::write(child.path.join("new.txt"), "child work").unwrap();
    for args in [
        vec!["describe", "-m", "owner"],
        vec!["commit", "-m", "owner"],
        vec!["new"],
    ] {
        jj_in(repo.path(), &args);
        snapshot(&child.path).unwrap();
        assert_eq!(
            std::fs::read_to_string(child.path.join("new.txt")).unwrap(),
            "child work"
        );
    }
}

#[test]
fn mint_base_is_a_sibling_of_parent_working_copy() {
    let root = native_repo();
    let child = mint(root.path(), "sibling-base", root.path(), None).unwrap();
    pollster::block_on(async {
        let ws = super::load(root.path()).unwrap();
        let repo = ws.repo_loader().load_at_head().await.unwrap();
        let parent = super::current(&repo, &ws).await.unwrap();
        let base = repo
            .store()
            .get_commit_async(&jj_lib::backend::CommitId::try_from_hex(&child.base.commit).unwrap())
            .await
            .unwrap();
        let child_ws = super::load(&child.path).unwrap();
        let head = super::current(&repo, &child_ws).await.unwrap();
        assert_eq!(base.parent_ids(), parent.parent_ids());
        assert_ne!(base.id(), parent.id());
        assert_eq!(base.tree().tree_ids(), parent.tree().tree_ids());
        assert_eq!(head.parent_ids(), &[base.id().clone()]);
        let git = repo
            .store()
            .backend()
            .downcast_ref::<jj_lib::git_backend::GitBackend>()
            .unwrap()
            .git_repo();
        let namespace = crate::occupancy_vcs::digest_text("sibling-base");
        let reference = format!("refs/plak/{}/bases/sibling-base", &namespace[..16]);
        assert!(git.try_find_reference(&reference).unwrap().is_some());
    });
    super::forget(root.path(), "sibling-base").unwrap();
    pollster::block_on(async {
        let ws = super::load(root.path()).unwrap();
        let repo = ws.repo_loader().load_at_head().await.unwrap();
        let git = repo
            .store()
            .backend()
            .downcast_ref::<jj_lib::git_backend::GitBackend>()
            .unwrap()
            .git_repo();
        let namespace = crate::occupancy_vcs::digest_text("sibling-base");
        assert!(
            git.try_find_reference(
                format!("refs/plak/{}/bases/sibling-base", &namespace[..16]).as_str()
            )
            .unwrap()
            .is_none()
        );
    });
}
#[test]
fn native_adoption_preserves_child_and_supports_followup() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let child = mint(repo.path(), "native", repo.path(), None).unwrap();
    std::fs::write(child.path.join("first.txt"), "first\n").unwrap();
    let first = snapshot(&child.path).unwrap();
    let landing = adopt(repo.path(), &child.path, &child.base.commit, &first, None).unwrap();
    assert!(landing.child_base.is_none());
    assert_eq!(
        std::fs::read_to_string(repo.path().join("first.txt")).unwrap(),
        "first\n"
    );
    assert_eq!(snapshot(&child.path).unwrap().content, first.content);
    std::fs::write(child.path.join("second.txt"), "second\n").unwrap();
    let second = snapshot(&child.path).unwrap();
    adopt(repo.path(), &child.path, &first.commit, &second, None).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("second.txt")).unwrap(),
        "second\n"
    );
    jj_in(repo.path(), &["status"]);
    jj_in(&child.path, &["status"]);
}
#[test]
fn parent_rewrite_refuses_recovery_and_preserves_dirty_and_untracked_files() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let child = mint(repo.path(), "preserve", repo.path(), None).unwrap();
    std::fs::write(child.path.join("SEED.txt"), "private child edit\n").unwrap();
    std::fs::write(child.path.join("untracked.txt"), "untracked child work\n").unwrap();
    jj_in(repo.path(), &["abandon", "-r", &child.base.commit]);
    let result = snapshot(&child.path);
    assert_eq!(
        std::fs::read_to_string(child.path.join("SEED.txt")).unwrap(),
        "private child edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(child.path.join("untracked.txt")).unwrap(),
        "untracked child work\n"
    );
    match result {
        Err(error) => assert_eq!(
            error.kind,
            crate::occupancy_vcs::VcsErrorKind::RecoveryRequired
        ),
        Ok(saved) => assert_eq!(snapshot(&child.path).unwrap().content, saved.content),
    }
}
fn native_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    pollster::block_on(jj_lib::workspace::Workspace::init_internal_git(
        &super::settings().unwrap(),
        repo.path(),
        "sha1".parse().unwrap(),
    ))
    .unwrap();
    std::fs::write(repo.path().join("SEED.txt"), "seed\n").unwrap();
    snapshot(repo.path()).unwrap();
    repo
}
#[test]
fn native_repository_snapshot_mint_and_adopt_need_no_cli_commands() {
    let repo = native_repo();
    let child = mint(repo.path(), "native-only", repo.path(), None).unwrap();
    std::fs::write(child.path.join("result.txt"), "done").unwrap();
    let result = snapshot(&child.path).unwrap();
    adopt(repo.path(), &child.path, &child.base.commit, &result, None).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("result.txt")).unwrap(),
        "done"
    );
}
#[test]
fn ancestor_rewrite_still_refuses_and_preserves_files() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let child = mint(repo.path(), "stale", repo.path(), None).unwrap();
    std::fs::write(child.path.join("SEED.txt"), "child dirty").unwrap();
    std::fs::write(child.path.join("untracked.txt"), "child untracked").unwrap();
    jj_in(repo.path(), &["edit", &child.base.commit]);
    std::fs::write(repo.path().join("SEED.txt"), "ancestor tree rewrite").unwrap();
    jj_in(repo.path(), &["status"]);
    for _ in 0..2 {
        let error = snapshot(&child.path).unwrap_err();
        assert_eq!(
            error.kind,
            crate::occupancy_vcs::VcsErrorKind::RecoveryRequired
        );
        assert_eq!(
            std::fs::read_to_string(child.path.join("SEED.txt")).unwrap(),
            "child dirty"
        );
        assert_eq!(
            std::fs::read_to_string(child.path.join("untracked.txt")).unwrap(),
            "child untracked"
        );
    }
}
#[test]
fn external_git_commit_import_and_native_adopt_preserve_staging() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@local",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    std::fs::write(repo.path().join("external.txt"), "external commit").unwrap();
    git(&["add", "external.txt"]);
    git(&["commit", "-m", "external"]);
    let git_head = git(&["rev-parse", "HEAD"]);
    std::fs::write(repo.path().join("staged.txt"), "staged").unwrap();
    git(&["add", "staged.txt"]);
    std::fs::write(repo.path().join("staged.txt"), "unstaged remainder").unwrap();
    let index = std::fs::read(repo.path().join(".git/index")).unwrap();
    let imported = snapshot(repo.path()).unwrap();
    pollster::block_on(async {
        let ws = super::load(repo.path()).unwrap();
        let view = ws.repo_loader().load_at_head().await.unwrap();
        let head = super::current(&view, &ws).await.unwrap();
        assert_eq!(
            head.parent_ids()[0].hex(),
            String::from_utf8(git_head.clone()).unwrap().trim()
        );
    });
    assert_eq!(
        std::fs::read(repo.path().join(".git/index")).unwrap(),
        index
    );
    let child = mint(repo.path(), "gitinterop", repo.path(), Some(&imported)).unwrap();
    std::fs::write(child.path.join("result.txt"), "child result").unwrap();
    let result = snapshot(&child.path).unwrap();
    adopt(repo.path(), &child.path, &child.base.commit, &result, None).unwrap();
    assert_eq!(git(&["rev-parse", "HEAD"]), git_head);
    assert_eq!(
        std::fs::read(repo.path().join(".git/index")).unwrap(),
        index
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("staged.txt")).unwrap(),
        "unstaged remainder"
    );
}
#[test]
fn publication_refuses_an_external_operation_after_evaluation() {
    if !jj_available() {
        return;
    }
    let repo = jj_repo();
    pollster::block_on(async {
        let ws = super::load(repo.path()).unwrap();
        let view = ws.repo_loader().load_at_head().await.unwrap();
        let tx = view.start_transaction();
        jj_in(repo.path(), &["describe", "-m", "external operation wins"]);
        let error = super::publish_checked(tx, "must not overwrite external operation")
            .await
            .unwrap_err();
        assert!(error.is::<super::RecoveryRequired>());
        let after = ws.repo_loader().load_at_head().await.unwrap();
        assert_eq!(
            super::current(&after, &ws).await.unwrap().description(),
            "external operation wins\n"
        );
    });
}
#[test]
fn secret_policy_allows_example_and_excludes_new_credentials() {
    let repo = native_repo();
    let before = snapshot(repo.path()).unwrap();
    for name in [
        ".env",
        ".ENV.production",
        "credentials.json",
        "id_ed25519",
        "private.pfx",
    ] {
        std::fs::write(repo.path().join(name), "fixture only").unwrap();
    }
    assert_eq!(snapshot(repo.path()).unwrap().content, before.content);
    std::fs::write(repo.path().join(".env.example"), "example").unwrap();
    assert_ne!(snapshot(repo.path()).unwrap().content, before.content);
}
#[test]
fn native_tree_identity_frames_order_and_conflict_labels() {
    let repo = native_repo();
    pollster::block_on(async {
        let ws = super::load(repo.path()).unwrap();
        let repo = ws.repo_loader().load_at_head().await.unwrap();
        let ids = vec![
            jj_lib::backend::TreeId::from_bytes(&[1; 20]),
            jj_lib::backend::TreeId::from_bytes(&[2; 20]),
            jj_lib::backend::TreeId::from_bytes(&[3; 20]),
        ];
        let tree = |ids: Vec<jj_lib::backend::TreeId>, labels: Vec<String>| {
            jj_lib::merged_tree::MergedTree::new(
                repo.store().clone(),
                jj_lib::merge::Merge::from_vec(ids),
                jj_lib::conflict_labels::ConflictLabels::from_vec(labels),
            )
        };
        let labels = vec!["left".into(), "base".into(), "right".into()];
        let first = super::tree_content(&tree(ids.clone(), labels.clone()));
        let mut reordered = ids.clone();
        reordered.swap(0, 2);
        assert_ne!(first, super::tree_content(&tree(reordered, labels)));
        assert_ne!(
            first,
            super::tree_content(&tree(
                ids,
                vec!["other".into(), "base".into(), "right".into()]
            ))
        );
        assert_eq!(
            first,
            "jj-tree-v1:b97ecdb4836cc06c05cd4317369828a992fdaef4564aac688ec6446642d2506a"
        );
    });
}
#[test]
fn candidate_ref_stays_exact_after_rewrite_and_workspace_removal() {
    let repo = native_repo();
    let child = mint(repo.path(), "pinned", repo.path(), None).unwrap();
    std::fs::write(child.path.join("candidate.txt"), "candidate").unwrap();
    let candidate = crate::occupancy_vcs::seal_candidate(
        &child.path,
        &super::super::occupancy_vcs::digest_text("fixture"),
        "candidate-pinned",
        &child.base.commit,
    )
    .unwrap();
    std::fs::write(child.path.join("candidate.txt"), "later work").unwrap();
    let changed = snapshot(&child.path).unwrap();
    assert!(
        super::pin(
            &child.path,
            &super::super::occupancy_vcs::digest_text("fixture"),
            "candidate-pinned",
            Some(&changed.commit)
        )
        .is_err()
    );
    super::forget(repo.path(), "pinned").unwrap();
    pollster::block_on(async {
        let ws = super::load(repo.path()).unwrap();
        let repo = ws.repo_loader().load_at_head().await.unwrap();
        let backend = repo
            .store()
            .backend()
            .downcast_ref::<jj_lib::git_backend::GitBackend>()
            .unwrap();
        let git = backend.git_repo();
        let reference = git
            .find_reference(
                super::super::occupancy_vcs::candidate_ref(
                    &super::super::occupancy_vcs::digest_text("fixture"),
                    "candidate-pinned",
                )
                .as_str(),
            )
            .unwrap();
        assert_eq!(
            reference.target().try_id().unwrap().to_string(),
            candidate.commit
        );
    });
}
