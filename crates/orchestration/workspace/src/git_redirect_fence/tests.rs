use super::{admit_git_invocation, install, install_jj, is_minted_occupancy_copy, shim_present};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A shim just written by another test thread can still be `ETXTBSY`: a
/// concurrent `fork` inherits the write descriptor until it execs. Retry
/// rather than let an unrelated race read as a fence verdict.
fn output_retrying(command: &mut Command) -> std::process::Output {
    for _ in 0..100 {
        match command.output() {
            Ok(output) => {
                if String::from_utf8_lossy(&output.stderr).contains("Text file busy") {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                }
                return output;
            }
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("shim did not run: {error}"),
        }
    }
    panic!("shim stayed busy");
}

fn git_repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "add", "SEED.txt"])
        .current_dir(root.path())
        .status();
    let _ = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.email=plak@local",
            "-c",
            "user.name=plak",
            "commit",
            "--no-gpg-sign",
            "-qm",
            "seed",
        ])
        .current_dir(root.path())
        .status();
    root
}

fn mint_child(parent: &Path, id: &str) -> PathBuf {
    let dest = parent.join(".plak/worktrees").join(id);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    assert!(
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("plak/{id}"),
                dest.to_str().unwrap(),
            ])
            .current_dir(parent)
            .status()
            .unwrap()
            .success()
    );
    dest
}

#[test]
fn jj_copy_git_shim_refuses_status_and_never_touches_launch() {
    if !jj_available() {
        return;
    }
    let launch = jj_repo();
    let child = mint_jj_child(launch.path(), "git-fence");
    let shim = install(&child).unwrap();
    std::fs::write(launch.path().join("SEED.txt"), "dirty launch").unwrap();
    // Keep a real launch index to detect accidental Git side effects.
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(launch.path())
            .status()
            .unwrap()
            .success()
    );
    let index = launch.path().join(".git/index");
    let before = std::fs::read(&index).ok();
    std::fs::create_dir(child.join("sub")).unwrap();
    for cwd in [&child, &child.join("sub")] {
        for args in [
            vec!["status"],
            vec!["stash"],
            vec!["checkout", "."],
            vec!["--git-dir=../../../.git", "status"],
        ] {
            let out = Command::new(&shim)
                .args(&args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(128), "{args:?}");
            assert!(String::from_utf8_lossy(&out.stderr).contains("Use jj"));
        }
    }
    for args in [vec!["--version"], vec!["version"], vec!["help"]] {
        assert!(
            Command::new(&shim)
                .args(args)
                .current_dir(&child)
                .status()
                .unwrap()
                .success()
        );
    }
    assert_eq!(
        std::fs::read_to_string(launch.path().join("SEED.txt")).unwrap(),
        "dirty launch"
    );
    assert_eq!(std::fs::read(&index).ok(), before);
    assert!(super::ignored_path_is_managed(
        &child,
        Path::new(".plak/bin/git"),
        None,
    ));
}

#[test]
fn jj_shim_refuses_nested_or_linked_workspace_discovery() {
    if !jj_available() {
        return;
    }
    let parent = jj_repo();
    let child = mint_jj_child(parent.path(), "discovery-child");
    let sibling = mint_jj_child(parent.path(), "discovery-sibling");
    let sibling_head = jj(
        &sibling,
        &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
    )
    .stdout;
    let base = String::from_utf8(
        jj(
            &child,
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        )
        .stdout,
    )
    .unwrap();
    let shim = install_jj(&child, base.trim()).unwrap();
    let sub = child.join("sub");
    let deep = sub.join("deep");
    std::fs::create_dir_all(&deep).unwrap();
    let run = |cwd: &Path, args: &[&str]| {
        Command::new(&shim)
            .args(args)
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(cwd)
            .output()
            .unwrap()
    };
    assert!(run(&deep, &["status"]).status.success());
    let refuse_new = |cwd: &Path| {
        let output = run(cwd, &["new", "root()"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("plak: jj-op refused"));
        assert_eq!(
            jj(
                &sibling,
                &["log", "-r", "@", "--no-graph", "-T", "commit_id"]
            )
            .stdout,
            sibling_head
        );
    };
    let nested = sub.join(".jj");
    std::os::unix::fs::symlink(sibling.join(".jj"), &nested).unwrap();
    refuse_new(&sub);
    refuse_new(&deep);
    std::fs::remove_file(&nested).unwrap();
    std::os::unix::fs::symlink(sub.join("missing"), &nested).unwrap();
    refuse_new(&deep);
    std::fs::remove_file(&nested).unwrap();
    std::fs::create_dir(&nested).unwrap();
    refuse_new(&deep);
    std::fs::remove_dir(&nested).unwrap();

    let marker = child.join(".jj");
    let saved = child.join("saved-jj");
    std::fs::rename(&marker, &saved).unwrap();
    refuse_new(&child);
    std::os::unix::fs::symlink(sibling.join(".jj"), &marker).unwrap();
    refuse_new(&child);
    std::fs::remove_file(&marker).unwrap();
    std::fs::rename(&saved, &marker).unwrap();
    assert!(run(&deep, &["status"]).status.success());
}

#[test]
fn jj_shim_refuses_repository_wide_operations() {
    if !jj_available() {
        return;
    }
    let parent = jj_repo();
    let child = mint_jj_child(parent.path(), "refusals");
    let sibling = mint_jj_child(parent.path(), "cluster-sibling");
    std::fs::write(sibling.join("keep.txt"), "sibling work").unwrap();
    let sibling_head = jj(
        &sibling,
        &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
    )
    .stdout;
    let launch_head = jj(
        parent.path(),
        &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
    )
    .stdout;
    let base = String::from_utf8(
        jj(
            &child,
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        )
        .stdout,
    )
    .unwrap();
    let shim = install_jj(&child, base.trim()).unwrap();
    let before = jj(
        parent.path(),
        &["op", "log", "--no-graph", "-n", "1", "-T", "id"],
    )
    .stdout;
    let glued = format!("-sR{}", sibling.display());
    let glued_log = format!("-GR{}", parent.path().display());
    let cases = [
        vec!["log", "-GR", parent.path().to_str().unwrap()],
        vec!["log", &glued_log],
        vec!["diff", &glued],
        vec!["prev", "-nR", sibling.to_str().unwrap()],
        vec!["next", "-nR", parent.path().to_str().unwrap()],
        vec!["--color", "never", "--at-op", "@", "undo"],
        vec!["log", "--config-file=ignored"],
        vec!["workspace", "remove", "plak-sibling"],
        vec!["tag", "track", "x@origin"],
        vec!["tag", "untrack", "x@origin"],
        vec!["dangerous-alias"],
        vec!["util", "backend"],
        vec!["--at-op", "@", "file", "track", ".env"],
        vec!["--at-op", "@", "file", "untrack", "file.txt"],
        vec!["--at-op", "@", "op", "integrate"],
        vec!["file", "track", ".env"],
        vec!["file", "track", "sub/ID_RSA"],
        vec!["file", "track", "root-glob:**"],
        vec!["file", "track"],
        vec!["undo"],
        vec!["redo"],
        vec!["--color", "never", "undo"],
        vec!["--color=never", "redo"],
        vec!["op", "restore", "@-"],
        vec!["operation", "revert", "@-"],
        vec!["op", "abandon", "@-"],
        vec!["op", "integrate"],
        vec!["util", "gc"],
        vec!["git", "push"],
        vec!["git", "fetch"],
        vec!["git", "import"],
        vec!["git", "export"],
        vec!["git", "remote", "list"],
        vec!["bookmark", "set", "main"],
        vec!["b", "s", "main"],
        vec!["bookmark", "delete", "main"],
        vec!["tag", "set", "test"],
        vec!["tag", "s", "test"],
        vec!["tag", "delete", "test"],
        vec!["tag", "d", "test"],
        vec!["workspace", "add", "elsewhere"],
        vec!["workspace", "forget"],
        vec!["workspace", "rename", "other"],
        vec!["workspace", "update-stale"],
        vec!["config", "set", "user.name", "other"],
        vec!["config", "s"],
        vec!["config", "unset"],
        vec!["config", "u"],
        vec!["config", "edit"],
        vec!["config", "e"],
        vec!["config", "gc"],
        vec!["gerrit"],
        vec!["--ignore-immutable", "describe"],
        vec!["--config-file", "config.toml", "status"],
        vec!["--config-file=config.toml", "status"],
        vec!["--config", "revset-aliases.x='all()'", "status"],
        vec!["--config=revset-aliases.x='all()'", "status"],
        vec!["--config", "snapshot.auto-track='all()'", "status"],
        vec!["--at-op", "@-", "describe"],
        vec!["--at-operation=@-", "commit"],
        vec!["-R", ".", "status"],
        vec!["--repository=.", "status"],
    ];
    for args in cases {
        let out = Command::new(&shim)
            .args(&args)
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(&child)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("jj-op refused"));
    }
    assert_eq!(
        jj(
            parent.path(),
            &["op", "log", "--no-graph", "-n", "1", "-T", "id"]
        )
        .stdout,
        before
    );
    assert_eq!(
        std::fs::read_to_string(sibling.join("keep.txt")).unwrap(),
        "sibling work"
    );
    assert_eq!(
        jj(
            &sibling,
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"]
        )
        .stdout,
        sibling_head
    );
    assert_eq!(
        jj(
            parent.path(),
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"]
        )
        .stdout,
        launch_head
    );
    assert!(super::ignored_path_is_managed(
        &child,
        Path::new(".plak/bin/jj"),
        Some(base.trim())
    ));
    assert!(!super::ignored_path_is_managed(
        &child,
        Path::new(".plak/bin/jj"),
        Some("wrong-base")
    ));
}

#[cfg(feature = "jj-lib")]
#[test]
fn jj_shim_immutability_and_secret_auto_track() {
    if !jj_available() {
        return;
    }
    let launch = crate::vcs_fixtures::jj_repo();
    let child = crate::native_jj::mint(launch.path(), "protected", launch.path(), None).unwrap();
    let sibling = crate::native_jj::mint(launch.path(), "sibling", launch.path(), None).unwrap();
    let shim = child.path.join(".plak/bin/jj");
    let before = jj(
        launch.path(),
        &["op", "log", "--no-graph", "-n", "1", "-T", "id"],
    )
    .stdout;
    for args in [
        vec!["workspace", "remove", sibling.branch.as_deref().unwrap()],
        vec!["tag", "track", "x@origin"],
    ] {
        let denied = Command::new(&shim)
            .args(args)
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(&child.path)
            .output()
            .unwrap();
        assert_eq!(denied.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&denied.stderr).contains("jj-op refused"));
    }
    assert!(sibling.path.join("SEED.txt").is_file());
    assert_eq!(
        jj(
            launch.path(),
            &["op", "log", "--no-graph", "-n", "1", "-T", "id"]
        )
        .stdout,
        before
    );
    let config = launch.path().join("aliases.toml");
    std::fs::write(&config, "[aliases]\ndangerous-alias=['undo']\nb=['undo']\n").unwrap();
    let denied = Command::new(&shim)
        .arg("dangerous-alias")
        .env("JJ_CONFIG", &config)
        .current_dir(&child.path)
        .output()
        .unwrap();
    assert_eq!(denied.status.code(), Some(1));
    let safe = Command::new(&shim)
        .args(["b", "list"])
        .env("JJ_CONFIG", &config)
        .current_dir(&child.path)
        .output()
        .unwrap();
    assert!(
        safe.status.success(),
        "{}",
        String::from_utf8_lossy(&safe.stderr)
    );
    std::fs::remove_file(config).unwrap();
    let root = String::from_utf8(
        jj(
            launch.path(),
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
        )
        .stdout,
    )
    .unwrap();
    let sibling_head = String::from_utf8(
        jj(
            &sibling.path,
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
        )
        .stdout,
    )
    .unwrap();
    for revision in [root.trim(), sibling_head.trim(), child.base.commit.as_str()] {
        for command in ["edit", "abandon", "describe"] {
            let mut args = vec![command, "-r", revision];
            if command == "describe" {
                args.extend(["-m", "forbidden"]);
            }
            let out = Command::new(&shim)
                .args(&args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(&child.path)
                .output()
                .unwrap();
            assert!(!out.status.success(), "{args:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("immutable"),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    std::fs::create_dir(child.path.join("sub")).unwrap();
    for name in [
        ".env",
        ".ENV.production",
        "id_rsa",
        "x.pem",
        "-id.pem",
        "sub/X.KEY",
        ".env.example",
        "ordinary.txt",
    ] {
        std::fs::write(child.path.join(name), "fixture").unwrap();
    }
    for args in [
        vec!["file", "track", ".env"],
        vec!["file", "track", "\".env\""],
        vec!["file", "track", "ordinary.txt&.env"],
        vec!["file", "track", "ordinary.txt", "--", "-id.pem"],
        vec!["file", "track", "all()"],
        vec!["--at-op", "@", "file", "track", "ordinary.txt"],
    ] {
        let denied = Command::new(&shim)
            .args(args)
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(&child.path)
            .output()
            .unwrap();
        assert_eq!(denied.status.code(), Some(1));
    }
    let tracked = Command::new(&shim)
        .args(["file", "track", "ordinary.txt", ".env.example"])
        .env("JJ_CONFIG", "/dev/null")
        .current_dir(&child.path)
        .output()
        .unwrap();
    assert!(
        tracked.status.success(),
        "{}",
        String::from_utf8_lossy(&tracked.stderr)
    );
    for args in [
        vec!["st"],
        vec!["log"],
        vec!["diff", "--from", "plak_base()"],
        vec!["op", "log"],
        vec!["bookmark", "list"],
        vec!["b", "l"],
        vec!["git", "root"],
        vec!["describe", "-m", "own"],
        vec!["commit", "-m", "own"],
        vec!["new"],
    ] {
        let out = Command::new(&shim)
            .args(&args)
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(child.path.join("sub"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let output = Command::new(&shim)
        .args(["file", "list"])
        .env("JJ_CONFIG", "/dev/null")
        .current_dir(&child.path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed = String::from_utf8(output.stdout).unwrap();
    for name in [
        ".env",
        ".ENV.production",
        "id_rsa",
        "x.pem",
        "-id.pem",
        "sub/X.KEY",
    ] {
        assert!(!listed.lines().any(|line| line == name), "{listed}");
    }
    assert!(listed.contains(".env.example"));
    crate::native_jj::snapshot(&child.path).unwrap();
}
#[test]
fn cwd_inside_copy_is_admitted() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    admit_git_invocation(&child, &child, &["git", "status"], None, None, None).unwrap();
    admit_git_invocation(
        &child,
        &child,
        &["git", "-C", ".", "status"],
        None,
        None,
        None,
    )
    .unwrap();
}

#[test]
fn minus_c_parent_is_denied() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let error = admit_git_invocation(
        &child,
        &child,
        &["git", "-C", parent.path().to_str().unwrap(), "status"],
        None,
        None,
        None,
    )
    .unwrap_err();
    assert!(error.detail.contains("escapes occupancy copy"));
}

#[test]
fn work_tree_parent_is_denied() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let parent_s = parent.path().to_str().unwrap();
    assert!(
        admit_git_invocation(
            &child,
            &child,
            &["git", &format!("--work-tree={parent_s}"), "status"],
            None,
            None,
            None,
        )
        .is_err()
    );
    assert!(
        admit_git_invocation(
            &child,
            &child,
            &["git", "--work-tree", parent_s, "status"],
            None,
            None,
            None,
        )
        .is_err()
    );
    assert!(
        admit_git_invocation(
            &child,
            &child,
            &["git", "status"],
            Some(parent_s),
            None,
            None
        )
        .is_err()
    );
}

#[test]
fn git_dir_parent_is_denied() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let parent_git = parent.path().join(".git");
    let parent_git_s = parent_git.to_str().unwrap();
    assert!(
        admit_git_invocation(
            &child,
            &child,
            &["git", &format!("--git-dir={parent_git_s}"), "status"],
            None,
            None,
            None,
        )
        .is_err()
    );
    assert!(
        admit_git_invocation(
            &child,
            &child,
            &["git", "status"],
            None,
            Some(parent_git_s),
            None
        )
        .is_err()
    );
}

#[test]
fn sibling_worktree_is_denied() {
    let parent = git_repo();
    let c1 = mint_child(parent.path(), "c1");
    let c2 = mint_child(parent.path(), "c2");
    assert!(
        admit_git_invocation(
            &c1,
            &c1,
            &["git", "-C", c2.to_str().unwrap(), "status"],
            None,
            None,
            None,
        )
        .is_err()
    );
}

#[test]
fn chained_minus_c_into_parent_is_denied() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    assert!(
        admit_git_invocation(
            &child,
            &child,
            &["git", "-C", "..", "-C", "..", "status"],
            None,
            None,
            None,
        )
        .is_err()
    );
}

#[test]
fn installed_shim_refuses_parent_and_allows_copy() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    assert!(shim_present(&child));
    let denied = output_retrying(
        Command::new(&shim)
            .args(["-C", parent.path().to_str().unwrap(), "status"])
            .current_dir(&child),
    );
    assert!(!denied.status.success());
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(stderr.contains("git-redirect refused"));
    let allowed = output_retrying(
        Command::new(&shim)
            .args(["status", "--porcelain"])
            .current_dir(&child),
    );
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
}

#[test]
fn installed_shim_refuses_work_tree_env() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    let denied = output_retrying(
        Command::new(&shim)
            .env("GIT_WORK_TREE", parent.path())
            .args(["status"])
            .current_dir(&child),
    );
    assert!(!denied.status.success());
}

#[test]
fn shell_path_hit_refuses_git_dash_c_parent() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    let bin = shim.parent().unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let denied = output_retrying(
        Command::new("/bin/sh")
            .args(["-c", &format!("git -C {} status", parent.path().display())])
            .env("PATH", path)
            .current_dir(&child),
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("git-redirect refused"));
}

#[test]
fn core_worktree_config_is_denied() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let parent_s = parent.path().to_str().unwrap();
    assert!(
        admit_git_invocation(
            &child,
            &child,
            &["git", "-c", &format!("core.worktree={parent_s}"), "status"],
            None,
            None,
            None,
        )
        .is_err()
    );
    assert!(
        admit_git_invocation(
            &child,
            &child,
            &["git", "status"],
            None,
            None,
            Some(&format!("core.worktree={parent_s}")),
        )
        .is_err()
    );
    let shim = install(&child).unwrap();
    let denied = output_retrying(
        Command::new(&shim)
            .args(["-c", &format!("core.worktree={parent_s}"), "status"])
            .current_dir(&child),
    );
    assert!(!denied.status.success());
}

#[test]
fn gitfile_git_dir_is_admitted() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    admit_git_invocation(
        &child,
        &child,
        &["git", "--git-dir=.git", "status"],
        None,
        None,
        None,
    )
    .unwrap();
}

#[test]
fn minted_copy_predicate_is_layout_not_path_inequality() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    assert!(is_minted_occupancy_copy(&child));
    assert!(!is_minted_occupancy_copy(parent.path()));
    assert!(!is_minted_occupancy_copy(&parent.path().join("src")));
}

#[test]
fn installed_shim_refuses_git_init_inside_copy() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    for args in [
        vec!["init"],
        vec!["init", "."],
        vec!["-c", "init.defaultBranch=main", "init"],
    ] {
        let denied = output_retrying(Command::new(&shim).args(&args).current_dir(&child));
        assert!(!denied.status.success(), "{args:?} was admitted");
        let stderr = String::from_utf8_lossy(&denied.stderr);
        assert!(
            stderr.contains("already a workspace of the launch repo"),
            "{stderr}"
        );
    }
    assert!(!child.join(".git").is_dir());
}

#[test]
fn installed_shim_refuses_clone_into_copy_root_but_allows_subdir_clone_args() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    let denied = output_retrying(
        Command::new(&shim)
            .args(["clone", parent.path().to_str().unwrap(), "."])
            .current_dir(&child),
    );
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("already a workspace of the launch repo")
    );
    let nested = output_retrying(
        Command::new(&shim)
            .args(["clone", parent.path().to_str().unwrap(), "vendor"])
            .current_dir(&child),
    );
    assert!(
        !String::from_utf8_lossy(&nested.stderr).contains("git-redirect refused"),
        "{}",
        String::from_utf8_lossy(&nested.stderr)
    );
}

#[test]
fn installed_shim_refuses_worktree_add_from_inside_copy() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    let denied = output_retrying(
        Command::new(&shim)
            .args(["worktree", "add", "sub"])
            .current_dir(&child),
    );
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("already a workspace of the launch repo")
    );
    let listed = output_retrying(
        Command::new(&shim)
            .args(["worktree", "list"])
            .current_dir(&child),
    );
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
}

#[test]
fn installed_shim_admits_subcommand_flags_that_look_like_globals() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    let listed = output_retrying(
        Command::new(&shim)
            .args(["log", "-1", "--format=%s", "-C"])
            .current_dir(&child),
    );
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
}

fn jj_available() -> bool {
    Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn jj(root: &Path, args: &[&str]) -> std::process::Output {
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
        .current_dir(root)
        .output()
        .unwrap()
}

fn jj_repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    assert!(jj(root.path(), &["git", "init"]).status.success());
    std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
    assert!(jj(root.path(), &["commit", "-m", "seed"]).status.success());
    root
}

fn mint_jj_child(parent: &Path, id: &str) -> PathBuf {
    let dest = parent.join(".plak/workspaces").join(id);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let out = jj(
        parent,
        &["workspace", "add", "--name", id, dest.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    dest
}

/// A child ran `jj git init` inside its copy and turned
/// it into a standalone repo the launch checkout could not see.
#[test]
fn installed_jj_shim_refuses_repo_creation_inside_copy() {
    if !jj_available() {
        return;
    }
    let parent = jj_repo();
    let child = mint_jj_child(parent.path(), "c1");
    let base = String::from_utf8(
        jj(
            &child,
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        )
        .stdout,
    )
    .unwrap();
    let shim = install_jj(&child, base.trim()).unwrap();
    for args in [
        vec!["init"],
        vec!["git", "init"],
        vec!["git", "init", "--colocate"],
        vec!["git", "clone", "https://example.invalid/x.git"],
    ] {
        let denied = output_retrying(
            Command::new(&shim)
                .args(&args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(&child),
        );
        assert!(!denied.status.success(), "{args:?} was admitted");
        let stderr = String::from_utf8_lossy(&denied.stderr);
        assert!(
            stderr.contains("jj-op refused")
                && stderr.contains("already a workspace of the launch repo"),
            "{args:?}: {stderr}"
        );
    }
    assert!(
        child.join(".jj").join("repo").is_file(),
        "copy stopped being a workspace pointer of the launch repo"
    );
}

#[test]
fn installed_jj_shim_still_refuses_workspace_mutation_and_parent_repo() {
    if !jj_available() {
        return;
    }
    let parent = jj_repo();
    let child = mint_jj_child(parent.path(), "c1");
    let base = String::from_utf8(
        jj(
            &child,
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        )
        .stdout,
    )
    .unwrap();
    let shim = install_jj(&child, base.trim()).unwrap();
    for args in [
        vec!["workspace", "add", "sub"],
        vec!["workspace", "forget", "c1"],
        vec!["workspace", "rename", "c2"],
    ] {
        let denied = output_retrying(
            Command::new(&shim)
                .args(&args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(&child),
        );
        assert!(!denied.status.success(), "{args:?} was admitted");
        assert!(
            String::from_utf8_lossy(&denied.stderr)
                .contains("workspace mutation escapes host mint")
        );
    }
    let denied = output_retrying(
        Command::new(&shim)
            .args(["-R", parent.path().to_str().unwrap(), "status"])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(&child),
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("repository escapes occupancy copy"));
}

#[test]
fn installed_jj_shim_admits_ordinary_commands_in_the_copy() {
    if !jj_available() {
        return;
    }
    let parent = jj_repo();
    let child = mint_jj_child(parent.path(), "c1");
    let base = String::from_utf8(
        jj(
            &child,
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        )
        .stdout,
    )
    .unwrap();
    let shim = install_jj(&child, base.trim()).unwrap();
    for args in [
        vec!["status"],
        vec!["workspace", "root"],
        vec!["describe", "-m", "init"],
    ] {
        let out = output_retrying(
            Command::new(&shim)
                .args(&args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(&child),
        );
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn login_shell_still_hits_shim_after_path_restore() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    let bin = shim.parent().unwrap();
    let wrapped = format!(
        "PATH={}:\"$PATH\"; export PATH\ngit -C {} status",
        bin.display(),
        parent.path().display()
    );
    let denied = output_retrying(
        Command::new("/bin/sh")
            .args(["-l", "-c", &wrapped])
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&child),
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("git-redirect refused"));
}

#[test]
fn shim_templates_render_the_frozen_scripts() {
    let copy = Path::new("/occ/copy");
    let git = Path::new("/usr/bin/git");
    let jj = Path::new("/usr/bin/jj");
    assert_eq!(
        super::shim_script(copy, git),
        include_str!("git_shim.frozen")
    );
    assert_eq!(
        super::jj_shim_script(copy, jj, "abc123"),
        include_str!("jj_shim.frozen")
    );
    assert_eq!(
        super::jj_copy_git_shim(Some(git)),
        include_str!("jj_copy_git_shim.frozen")
    );
}
