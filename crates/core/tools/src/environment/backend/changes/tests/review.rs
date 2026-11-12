use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::time::Duration;

pub(super) fn index_witness(root: &Path) -> (u64, Vec<u8>) {
    let path = root.join(".git/index");
    (
        std::fs::metadata(&path).unwrap().ino(),
        std::fs::read(path).unwrap(),
    )
}

fn executor(path: &std::ffi::OsStr) -> process::Executor {
    process::Executor::new(
        tokio::time::Instant::now() + Duration::from_secs(5),
        CancellationToken::new(),
        path,
    )
}

#[tokio::test]
async fn relative_and_empty_search_entries_never_execute_repository_git() {
    const CHILD: &str = "PLAK_CHANGES_PATH_FIXTURE";
    if let Some(root) = std::env::var_os(CHILD) {
        let observed = status(&environment(Path::new(&root))).await;
        assert_eq!(observed.repository.workspace, Path::new(&root));
        return;
    }
    let root = checkout();
    let marker = root.path().join("marker");
    std::fs::create_dir(root.path().join("bin")).unwrap();
    for file in [root.path().join("git"), root.path().join("bin/git")] {
        std::fs::write(
            &file,
            format!("#!/bin/sh\ntouch '{}'\nexit 99\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let safe = std::env::join_paths(
        std::env::split_paths(&std::env::var_os("PATH").unwrap()).filter(|path| path.is_absolute()),
    )
    .unwrap();
    for prefix in [":", "bin:", "./bin:"] {
        let mut path = std::ffi::OsString::from(prefix);
        path.push(&safe);
        let control = executor(&path);
        let result = control
            .run(root.path(), None, None, &["--version"], 256)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.bytes.starts_with(b"git version "));
        assert!(!marker.exists());
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "environment::backend::changes::tests::review::relative_and_empty_search_entries_never_execute_repository_git"])
            .env(CHILD, root.path()).env("PATH", control_path(prefix, &safe))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!marker.exists());
    }
    let path = std::env::join_paths([root.path().join("bin")]).unwrap();
    assert_eq!(
        executor(&path).resolve(root.path(), None).await,
        Err(ChangesRefusal::UnsafeRepository)
    );
    let subdirectory = root.path().join("sub");
    std::fs::create_dir(&subdirectory).unwrap();
    let path = std::env::join_paths([root.path().join("bin")]).unwrap();
    assert_eq!(
        executor(&path)
            .resolve(&subdirectory, Some(root.path()))
            .await,
        Err(ChangesRefusal::UnsafeRepository)
    );
    assert!(!marker.exists());
}

fn control_path(prefix: &str, safe: &std::ffi::OsStr) -> std::ffi::OsString {
    let mut path = std::ffi::OsString::from(prefix);
    path.push(safe);
    path
}

#[tokio::test]
async fn staged_then_restored_worktree_never_refreshes_or_replaces_index() {
    let root = checkout();
    std::fs::write(root.path().join("file"), "staged\n").unwrap();
    git(root.path(), &["add", "file"]);
    std::fs::write(root.path().join("file"), "before\n").unwrap();
    git(root.path(), &["config", "diff.autoRefreshIndex", "true"]);
    let before = index_witness(root.path());
    let environment = environment(root.path());
    let observed = status(&environment).await;
    assert_eq!(observed.entries[0].state, "MM");
    assert!(matches!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Diff(_)
    ));
    assert_eq!(index_witness(root.path()), before);
    assert!(!root.path().join(".git/index.lock").exists());
}

#[test]
fn repository_ownership_and_all_metadata_redirects_are_refused() {
    let root = checkout();
    let root = root.path().canonicalize().unwrap();
    let owner = rustix::process::geteuid().as_raw();
    assert!(repository::identify_owned(&root, None, &[], owner).is_ok());
    assert_eq!(
        repository::identify_owned(&root, None, &[], owner.wrapping_add(1)),
        Err(ChangesRefusal::UnsafeRepository)
    );
    for name in ["config", "HEAD", "objects/pack", "packed-refs"] {
        let path = root.join(".git").join(name);
        let saved = path.with_extension("saved");
        let existed = path.exists();
        if existed {
            std::fs::rename(&path, &saved).unwrap();
        }
        symlink(&saved, &path).unwrap();
        assert_eq!(
            repository::identify(&root, None, &[]),
            Err(ChangesRefusal::UnsafeRepository),
            "{name}"
        );
        std::fs::remove_file(&path).unwrap();
        if existed {
            std::fs::rename(&saved, &path).unwrap();
        }
    }
    std::fs::write(root.join(".git/objects/info/alternates"), "").unwrap();
    assert_eq!(
        repository::identify(&root, None, &[]),
        Err(ChangesRefusal::UnsafeRepository)
    );
}

#[tokio::test]
async fn unattributed_default_textconv_cannot_execute() {
    let root = checkout();
    let marker = root.path().join("marker");
    git(
        root.path(),
        &[
            "config",
            "diff.default.textconv",
            &format!("touch {}", marker.display()),
        ],
    );
    std::fs::write(root.path().join("file"), "changed\n").unwrap();
    let environment = environment(root.path());
    let observed = status(&environment).await;
    assert!(matches!(
        ask(&environment, diff_request(&observed, 0)).await,
        ChangesAnswer::Diff(ChangesDiff::Patch { .. })
    ));
    assert!(!marker.exists());
}

#[tokio::test]
async fn protocol_allowlist_and_no_lazy_fetch_each_block_a_real_local_fetch() {
    let source = checkout();
    let oid = String::from_utf8(git(source.path(), &["rev-parse", "HEAD:file"])).unwrap();
    let oid = oid.trim();
    for defence in ["protocol", "lazy", "neither"] {
        let target = tempfile::tempdir().unwrap();
        git(target.path(), &["init", "--quiet"]);
        git(
            target.path(),
            &[
                "config",
                "remote.origin.url",
                source.path().to_str().unwrap(),
            ],
        );
        git(target.path(), &["config", "remote.origin.promisor", "true"]);
        let path = std::env::var_os("PATH").unwrap();
        let executor = executor(&path);
        let program = executor.resolve(target.path(), None).await.unwrap();
        let mut command = process::command(&program, target.path(), &path);
        command.args(["-c", "protocol.file.allow=always", "cat-file", "-e", oid]);
        if defence != "protocol" {
            command.env("GIT_ALLOW_PROTOCOL", "file");
        }
        if defence != "lazy" {
            command.env_remove("GIT_NO_LAZY_FETCH");
        }
        let result = executor.output(command, 1024).await.unwrap();
        assert_eq!(result.success, defence == "neither", "{defence}");
        let result = git(target.path(), &["count-objects", "-v"]);
        if defence != "neither" {
            assert!(result.starts_with(b"count: 0\nsize: 0\nin-pack: 0\n"));
        }
    }
}

#[test]
fn default_product_home_stays_protected_when_the_configured_home_moves() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let product = home.join("alternate");
    let roots =
        repository::protected_roots_from(Some(home.as_os_str()), Some(product.as_os_str()), None)
            .unwrap();
    assert!(roots.contains(&home.join(".plak")));
    assert!(roots.contains(&product));
}

#[tokio::test]
async fn unreadably_long_head_is_refused_instead_of_returning_an_unbounded_label() {
    let root = checkout();
    let branch = std::iter::repeat_n("a".repeat(199), 21)
        .collect::<Vec<_>>()
        .join("/");
    let branch = &branch[..4080];
    std::fs::write(
        root.path().join(".git/HEAD"),
        format!("ref: refs/heads/{branch}\n"),
    )
    .unwrap();
    assert_eq!(
        ask(&environment(root.path()), ChangesRequest::status()).await,
        ChangesAnswer::Refused(ChangesRefusal::UnsafeRepository)
    );
}
