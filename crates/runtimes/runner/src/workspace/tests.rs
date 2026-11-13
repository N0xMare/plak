use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn protocol_eight_workspace_payloads_are_pinned() {
    assert_eq!(plakous_runner_protocol::PROTOCOL_VERSION, 8);
    let mut samples = vec![
        serde_json::to_value(WorkspaceOptions {
            roots: Some(vec!["/workspace".into()]),
            default_workspace: Some("/workspace".into()),
            copies: true,
        })
        .unwrap(),
        serde_json::to_value(WorkspaceOptions {
            roots: None,
            default_workspace: Some("/startup".into()),
            copies: false,
        })
        .unwrap(),
        serde_json::to_value(ValidationRequest {
            path: "/workspace".into(),
            require_canonical: true,
        })
        .unwrap(),
        serde_json::to_value(Ok::<_, WorkspaceRefusal>("/workspace")).unwrap(),
    ];
    for head in [
        RepositoryHead::Branch {
            name: "topic".into(),
        },
        RepositoryHead::Detached,
        RepositoryHead::Unborn { name: "new".into() },
        RepositoryHead::Unavailable,
    ] {
        let commit = matches!(
            head,
            RepositoryHead::Branch { .. } | RepositoryHead::Detached
        )
        .then(|| COMMIT.into());
        samples.push(
            serde_json::to_value(WorkspaceDiscovery {
                workspaces: vec![DiscoveredWorkspace {
                    path: "/workspace/repo".into(),
                    repo: Some(RepositoryMetadata {
                        name: "repo".into(),
                        head,
                        commit,
                    }),
                }],
                truncated: false,
            })
            .unwrap(),
        );
    }
    for refusal in [
        WorkspaceRefusal::InvalidPath,
        WorkspaceRefusal::NotCanonical {
            canonical: "/workspace".into(),
        },
        WorkspaceRefusal::NotFound,
        WorkspaceRefusal::NotDirectory,
        WorkspaceRefusal::OutsideRoots,
        WorkspaceRefusal::ProductHome,
        WorkspaceRefusal::PermissionDenied,
    ] {
        samples.push(serde_json::to_value(Err::<String, _>(refusal)).unwrap());
    }
    let pinned = include_str!("pinned/v8.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        samples, pinned,
        "workspace payload changes require a runner protocol bump, never editing an existing version pin"
    );
}

fn directory(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

fn checkout(parent: &Path, name: &str, head: &str) -> PathBuf {
    let path = directory(parent, name);
    let git = directory(&path, ".git");
    std::fs::write(git.join("HEAD"), head).unwrap();
    path
}

#[test]
fn inadmissible_recents_never_add_discovery_parents() {
    let root = tempfile::tempdir().unwrap();
    let missing_parent = directory(root.path(), "missing-parent");
    checkout(&missing_parent, "unrelated", COMMIT);
    let containing = directory(root.path(), "containing");
    let home = directory(&containing, "home");
    checkout(root.path(), "outside", COMMIT);
    let admitted_parent = directory(root.path(), "admitted-parent");
    let admitted = checkout(&admitted_parent, "recent", COMMIT);
    let sibling = checkout(&admitted_parent, "sibling", COMMIT);
    let catalog = WorkspaceCatalog::local(
        None,
        &[missing_parent.join("missing"), containing, admitted.clone()],
        &home,
    )
    .unwrap();
    assert_eq!(catalog.bases, [admitted_parent]);
    let found = catalog
        .discover()
        .workspaces
        .into_iter()
        .map(|row| row.path)
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [admitted.to_str().unwrap(), sibling.to_str().unwrap()]
    );
}

#[test]
fn explicit_local_defaults_preserve_typed_home_and_alias_refusals() {
    let root = tempfile::tempdir().unwrap();
    let home = directory(root.path(), "home");
    assert!(matches!(
        WorkspaceCatalog::local(Some(&home), &[], &home),
        Err(WorkspaceRefusal::ProductHome)
    ));
    let workspace = directory(root.path(), "workspace");
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    assert!(
        matches!(WorkspaceCatalog::local(Some(&alias), &[], &home), Err(WorkspaceRefusal::NotCanonical { canonical }) if canonical == workspace.to_str().unwrap())
    );
}

#[test]
fn discovery_reports_closed_head_states_and_does_not_descend_into_checkouts() {
    let home = tempfile::tempdir().unwrap();
    let protected = directory(home.path(), "protected");
    let root = directory(home.path(), "root");
    let branch = checkout(&root, "branch", "ref: refs/heads/topic\n");
    directory(&branch.join(".git"), "refs/heads");
    std::fs::write(branch.join(".git/refs/heads/topic"), COMMIT).unwrap();
    checkout(&root, "detached", COMMIT);
    checkout(&root, "unborn", "ref: refs/heads/new\n");
    checkout(&root, "unavailable", "invalid");
    checkout(&branch, "nested", COMMIT);
    let catalog = WorkspaceCatalog::machine(vec![root], &protected, true).unwrap();
    let discovery = catalog.discover();
    assert_eq!(discovery.workspaces.len(), 4);
    let metadata = discovery
        .workspaces
        .iter()
        .map(|workspace| workspace.repo.as_ref().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        metadata[0].head,
        RepositoryHead::Branch {
            name: "topic".into()
        }
    );
    assert_eq!(metadata[0].commit.as_deref(), Some(COMMIT));
    assert_eq!(metadata[1].head, RepositoryHead::Detached);
    assert_eq!(metadata[2].head, RepositoryHead::Unavailable);
    assert_eq!(
        metadata[3].head,
        RepositoryHead::Unborn { name: "new".into() }
    );
    assert!(metadata[2].commit.is_none() && metadata[3].commit.is_none());
}

#[test]
fn discovery_gitdir_and_commondir_cannot_escape_roots() {
    let home = tempfile::tempdir().unwrap();
    let protected = directory(home.path(), "protected");
    let root = directory(home.path(), "root");
    let outside = directory(home.path(), "outside");
    std::fs::write(outside.join("HEAD"), COMMIT).unwrap();
    let worktree = directory(&root, "worktree");
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", outside.display()),
    )
    .unwrap();
    let catalog = WorkspaceCatalog::machine(vec![root.clone()], &protected, false).unwrap();
    let discovery = catalog.discover();
    assert_eq!(
        discovery.workspaces[0].repo.as_ref().unwrap().head,
        RepositoryHead::Unavailable
    );
    let git = directory(&root, "metadata");
    std::fs::write(git.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
    std::fs::write(
        git.join("commondir"),
        outside.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", git.display()),
    )
    .unwrap();
    let catalog = WorkspaceCatalog::machine(vec![root], &protected, false).unwrap();
    assert_eq!(
        catalog.discover().workspaces[0].repo.as_ref().unwrap().head,
        RepositoryHead::Unavailable
    );
}

#[cfg(unix)]
#[test]
fn discovery_never_waits_for_a_writer_of_special_git_metadata() {
    let home = tempfile::tempdir().unwrap();
    let protected = directory(home.path(), "protected");
    let root = directory(home.path(), "root");
    let repository = checkout(&root, "fifo", "unused");
    let head = repository.join(".git/HEAD");
    std::fs::remove_file(&head).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&head)
            .status()
            .unwrap()
            .success()
    );
    let catalog = WorkspaceCatalog::machine(vec![root], &protected, false).unwrap();
    let discovery = catalog.discover();
    assert_eq!(
        discovery.workspaces[0].repo.as_ref().unwrap().head,
        RepositoryHead::Unavailable
    );
}

#[test]
fn validation_checks_containment_before_missing_symlink_target() {
    let home = tempfile::tempdir().unwrap();
    let protected = directory(home.path(), "protected");
    let root = directory(home.path(), "root");
    let catalog = WorkspaceCatalog::machine(vec![root.clone()], &protected, false).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(home.path().join("missing"), root.join("escape")).unwrap();
        assert_eq!(
            catalog.validate(root.join("escape").to_str().unwrap(), true),
            Err(WorkspaceRefusal::OutsideRoots)
        );
        std::os::unix::fs::symlink(&root, root.join("alias")).unwrap();
        assert_eq!(
            catalog.validate(root.join("alias").to_str().unwrap(), true),
            Err(WorkspaceRefusal::NotCanonical {
                canonical: root.to_str().unwrap().into()
            })
        );
        assert_eq!(
            catalog.validate(root.join("alias").to_str().unwrap(), false),
            Ok(root.to_str().unwrap().into())
        );
    }
    assert_eq!(
        catalog.validate(root.join("missing").to_str().unwrap(), true),
        Err(WorkspaceRefusal::NotFound)
    );
    assert_eq!(
        catalog.validate("relative", true),
        Err(WorkspaceRefusal::InvalidPath)
    );
    assert_eq!(
        catalog.validate(root.join("../root").to_str().unwrap(), true),
        Err(WorkspaceRefusal::InvalidPath)
    );
    assert!(WorkspaceCatalog::machine(vec![home.path().to_owned()], &protected, false).is_err());
}

#[test]
fn discovery_bounds_depth_count_and_cache_without_changing_admission() {
    let home = tempfile::tempdir().unwrap();
    let protected = directory(home.path(), "protected");
    let root = directory(home.path(), "root");
    for number in 0..205 {
        checkout(&root, &format!("repo-{number:03}"), COMMIT);
    }
    checkout(&root, "one/two/three", COMMIT);
    let catalog = WorkspaceCatalog::machine(vec![root.clone()], &protected, false).unwrap();
    let snapshot = catalog.discover();
    assert!(snapshot.truncated);
    assert_eq!(snapshot.workspaces.len(), 200);
    assert!(
        snapshot
            .workspaces
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path)
    );
    assert!(
        !snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.path.ends_with("three"))
    );
    checkout(&root, "aaa", COMMIT);
    assert_eq!(catalog.discover(), snapshot);
    assert!(
        catalog
            .validate(root.join("aaa").to_str().unwrap(), true)
            .is_ok()
    );
    let local =
        WorkspaceCatalog::local(Some(&root), &[protected.join("recent")], &protected).unwrap();
    assert_eq!(local.options().roots, None);
    assert_eq!(
        local.validate(protected.to_str().unwrap(), true),
        Err(WorkspaceRefusal::ProductHome)
    );
    assert!(local.validate(home.path().to_str().unwrap(), true).is_err());
}

#[test]
fn roots_and_default_are_daemon_facts_and_gitfiles_inside_roots_work() {
    let home = tempfile::tempdir().unwrap();
    let protected = directory(home.path(), "protected");
    let root = directory(home.path(), "root");
    let git = directory(&root, "metadata");
    std::fs::write(git.join("HEAD"), COMMIT).unwrap();
    let worktree = directory(&root, "worktree");
    std::fs::write(worktree.join(".git"), "gitdir: ../metadata\n").unwrap();
    let catalog = WorkspaceCatalog::machine(vec![root.clone()], &protected, true).unwrap();
    assert_eq!(
        catalog.options(),
        WorkspaceOptions {
            roots: Some(vec![root.to_str().unwrap().into()]),
            default_workspace: Some(root.to_str().unwrap().into()),
            copies: true
        }
    );
    assert_eq!(
        catalog.discover().workspaces[0].repo.as_ref().unwrap().head,
        RepositoryHead::Detached
    );
}

#[test]
fn a_home_started_service_has_no_implicit_workspace_and_keeps_the_home_protected() {
    let home = tempfile::tempdir().unwrap();
    let protected = directory(home.path(), "protected");
    let workspace = directory(home.path(), "workspace");
    let catalog = WorkspaceCatalog::local(None, &[], &protected).unwrap();
    assert_eq!(catalog.options().default_workspace, None);
    assert_eq!(catalog.options().roots, None);
    assert_eq!(catalog.discover().workspaces, []);
    assert_eq!(
        catalog.validate(protected.to_str().unwrap(), true),
        Err(WorkspaceRefusal::ProductHome)
    );
    assert_eq!(
        catalog.validate(workspace.to_str().unwrap(), true),
        Ok(workspace.to_str().unwrap().into())
    );
}
