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
    let stuck = format!("-C{}", parent.path().display());
    assert!(
        admit_git_invocation(&child, &child, &["git", &stuck, "status"], None, None, None).is_err()
    );
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
fn installed_shim_keeps_git_dir_and_cwd_forms_inside_the_copy_and_guarded() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    let own_dir = git_stdout(&child, &["rev-parse", "--absolute-git-dir"]);
    let own_dir = own_dir.trim();
    let parent_dir = parent.path().join(".git");
    let own_option = format!("--git-dir={own_dir}");
    let parent_option = format!("--git-dir={}", parent_dir.display());
    let before = coordination_refs(parent.path());
    for (args, env_dir, marker) in [
        (
            vec!["--git-dir", own_dir, "update-ref", "refs/plak/pin", "HEAD"],
            None,
            "git-guard refused",
        ),
        (
            vec![own_option.as_str(), "update-ref", "refs/plak/pin", "HEAD"],
            None,
            "git-guard refused",
        ),
        (
            vec!["update-ref", "refs/plak/pin", "HEAD"],
            Some(own_dir),
            "git-guard refused",
        ),
        (
            vec!["-C", parent.path().to_str().unwrap(), "status"],
            None,
            "git-redirect refused",
        ),
        (
            vec![parent_option.as_str(), "status"],
            None,
            "git-redirect refused",
        ),
        (
            vec!["--git-dir", parent_dir.to_str().unwrap(), "status"],
            None,
            "git-redirect refused",
        ),
        (vec!["status"], parent_dir.to_str(), "git-redirect refused"),
    ] {
        let mut command = Command::new(&shim);
        command.args(&args).current_dir(&child);
        if let Some(dir) = env_dir {
            command.env("GIT_DIR", dir);
        }
        let output = output_retrying(&mut command);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(128), "{args:?}: {stderr}");
        assert!(stderr.contains(marker), "{args:?}: {stderr}");
        assert_eq!(coordination_refs(parent.path()), before);
    }
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
    for args in [
        vec![
            "git",
            "--config-env=core.worktree=UNSET_GIT_WORKTREE",
            "status",
        ],
        vec![
            "git",
            "--attr-source",
            "HEAD",
            "-c",
            "core.worktree=..",
            "status",
        ],
    ] {
        assert!(admit_git_invocation(&child, &child, &args, None, None, None).is_err());
    }
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
        super::shim_script(copy, git, "plak/copy"),
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

fn git_ok(cwd: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap()
            .success(),
        "git {args:?}"
    );
}

fn coordination_refs(parent: &Path) -> Vec<u8> {
    Command::new("git")
        .args(["for-each-ref", "refs/plak", "refs/heads/plak", "refs/stash"])
        .current_dir(parent)
        .output()
        .unwrap()
        .stdout
}

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Refuses writes to pins, other copies' branches, the stash, and retention.
#[test]
fn guard_refuses_coordination_writes() {
    let parent = git_repo();
    git_ok(
        parent.path(),
        &["config", "user.email", "plak@example.test"],
    );
    git_ok(parent.path(), &["config", "user.name", "plak"]);
    std::fs::write(parent.path().join("f"), "file\n").unwrap();
    git_ok(parent.path(), &["add", "f"]);
    git_ok(parent.path(), &["commit", "-qm", "file"]);
    let head = git_stdout(parent.path(), &["rev-parse", "HEAD"]);
    let head = head.trim();
    let parent_branch = git_stdout(parent.path(), &["symbolic-ref", "--short", "HEAD"]);
    let parent_ref = format!("refs/heads/{}", parent_branch.trim());
    let uppercase_parent_ref = parent_ref.to_uppercase();
    git_ok(parent.path(), &["update-ref", "refs/plak/pin", head]);
    git_ok(parent.path(), &["branch", "plak/c2"]);
    let child = mint_child(parent.path(), "c1");
    std::fs::write(child.join("wip.txt"), "wip\n").unwrap();
    git_ok(&child, &["add", "wip.txt"]);
    git_ok(&child, &["stash", "push", "-qm", "wip"]);
    git_ok(
        &child,
        &["remote", "add", "self", parent.path().to_str().unwrap()],
    );
    git_ok(&child, &["config", "url...insteadOf", "local-copy:"]);
    git_ok(&child, &["config", "url...pushInsteadOf", "push-copy:"]);
    git_ok(
        &child,
        &["remote", "add", "push-rewrite-self", "push-copy:"],
    );
    std::fs::create_dir(child.join("update-ref")).unwrap();
    let shim = install(&child).unwrap();
    let refs_before = coordination_refs(parent.path());
    let index_before = std::fs::read(parent.path().join(".git/index")).unwrap();
    let parent_path = parent.path().display().to_string();
    let file_remote = format!("file://{parent_path}");
    let child_remote = child.display().to_string();
    let child_file_remote = format!("file://{child_remote}");
    let empty_pathspec = child.join("empty-pathspec");
    std::fs::write(&empty_pathspec, "").unwrap();
    let empty_pathspec_option = format!("--pathspec-from-file={}", empty_pathspec.display());
    let mut refused: Vec<Vec<String>> = [
        vec!["update-ref", "refs/plak/pin", head],
        vec!["update-ref", "-d", "refs/stash"],
        vec!["update-ref", "refs/heads/plak/c2", head],
        vec!["update-ref", parent_ref.as_str(), head],
        vec!["update-ref", uppercase_parent_ref.as_str(), head],
        vec![
            "-C",
            child_remote.as_str(),
            "update-ref",
            "refs/plak/pin",
            head,
        ],
        vec!["symbolic-ref", "refs/heads/plak/c2", "refs/heads/plak/c1"],
        vec!["branch", "-d", "plak/c2"],
        vec!["branch", "-f", "Plak/c2"],
        vec!["branch", "-v", "-f", "plak/c2", "HEAD"],
        vec!["branch", "-qv", "-f", "plak/c2", "HEAD"],
        vec!["branch", "--list", "--no-list", "-f", "plak/c2", "HEAD"],
        vec![
            "branch",
            "--show-current",
            "--no-show-current",
            "-f",
            "plak/c2",
            "HEAD",
        ],
        vec!["branch", "plak/c3"],
        vec!["checkout", "-b", "plak/c3"],
        vec!["switch", "-c", "plak/c4"],
        vec!["fetch", "origin", "HEAD:refs/heads/plak/c2"],
        vec!["push", ".", "HEAD:refs/plak/pin"],
        vec!["push", ".", "--delete", "refs/plak/pin"],
        vec!["push", ".", "refs/plak/pin", "--delete"],
        vec!["push", ".", "-d", "refs/heads/plak/c2"],
        vec!["push", ".", "--force-with-lease", "HEAD:refs/plak/pin"],
        vec!["push", "--repo", "origin", ".", "HEAD:refs/plak/pin"],
        vec!["push", "--repo=origin", "./", "HEAD:refs/plak/pin"],
        vec!["push", ".", "HEAD:refs/plak/pin", "--repo", "origin"],
        vec![
            "push",
            ".",
            "--force-with-lease=refs/plak/pin",
            "HEAD:refs/plak/pin",
        ],
        vec!["push", "./", "HEAD:refs/plak/pin"],
        vec!["push", parent_path.as_str(), "HEAD:refs/plak/pin"],
        vec!["push", file_remote.as_str(), "HEAD:refs/plak/pin"],
        vec!["push", child_remote.as_str(), "HEAD:refs/plak/pin"],
        vec!["push", child_file_remote.as_str(), "HEAD:refs/plak/pin"],
        vec!["switch", "--create", "plak/c3"],
        vec!["switch", "--create=plak/c3"],
        vec!["switch", "--force-create", "plak/c4"],
        vec!["switch", "--force-create=plak/c4"],
        vec!["checkout", "-bplak/c5"],
        vec!["checkout", "-Bplak/c5"],
        vec!["checkout", "--orphan=plak/c5"],
        vec!["switch", "-cplak/c6"],
        vec!["switch", "-Cplak/c6"],
        vec!["update-ref", "refs/PLAK/pin", head],
        vec!["branch", "Plak/c2"],
        vec!["update-ref", "refs/heads/Plak/c2", head],
        vec!["symbolic-ref", "refs/heads/feature", "refs/plak/pin"],
        vec![
            "symbolic-ref",
            "-m",
            "reason",
            "refs/heads/feature",
            "refs/plak/pin",
        ],
        vec!["symbolic-ref", "-d", "refs/plak/pin"],
        vec!["fetch", "--refmap", "HEAD:refs/plak/pin", "origin"],
        vec!["fetch", "--refmap=HEAD:refs/plak/pin", "origin"],
        vec!["pull", "origin", "HEAD:refs/plak/pin"],
        vec!["stash"],
        vec!["stash", "push"],
        vec!["gc", "--prune=now"],
        vec!["prune"],
        vec!["prune", "-n", "--no-dry-run"],
        vec!["reflog", "expire", "--all"],
        vec!["reflog", "delete", "HEAD@{0}"],
        vec!["worktree", "remove", parent_path.as_str()],
        vec!["worktree", "move", parent_path.as_str(), "elsewhere"],
        vec!["worktree", "lock", parent_path.as_str()],
        vec!["worktree", "unlock", parent_path.as_str()],
        vec!["worktree", "prune"],
        vec!["worktree", "prune", "--expire=now"],
        vec!["worktree", "prune", "-n", "--no-dry-run"],
        vec!["worktree", "repair", parent_path.as_str()],
        vec!["worktree", "repair"],
        vec![
            "--work-tree",
            "update-ref",
            "update-ref",
            "-d",
            "refs/plak/pin",
        ],
        vec!["-C", "update-ref", "update-ref", "-d", "refs/plak/pin"],
        vec!["--attr-source", "HEAD", "branch", "-D", "plak/c2"],
        vec!["--attr-source", "HEAD", "stash", "drop"],
        vec![
            "--attr-source",
            "HEAD",
            "push",
            ".",
            "--delete",
            "refs/plak/pin",
        ],
        vec!["--attr-source", "HEAD", "gc", "--prune=now"],
        vec!["fetch", "--prune", ".", "+refs/nothing/*:refs/*"],
        vec!["fetch", "--prune", ".", "+refs/*:refs/*"],
        vec!["fetch", "--prune", ".", "+refs/nothing/*:refs/tags/*"],
        vec!["fetch", ".", "+refs/nothing/*:refs/heads/*"],
        vec!["fetch", ".", "+refs/nothing/*:*"],
        vec!["push", "--prune", ".", "refs/nothing/*:refs/*"],
        vec!["push", ".", "refs/heads/*:refs/*"],
        vec!["push", ".", "--mirror"],
        vec!["update-ref", "main-worktree/HEAD", head],
        vec!["update-ref", "worktrees/c2/HEAD", head],
        vec!["symbolic-ref", "HEAD", parent_ref.as_str()],
        vec!["symbolic-ref", "worktrees/c2/HEAD", "refs/heads/feature"],
        vec!["switch", "--ignore-other-worktrees", "plak/c2"],
        vec!["checkout", "--ignore-other-worktrees", parent_branch.trim()],
        vec!["checkout", "plak/c2"],
        vec!["checkout", "-b", "plak/new", "--"],
        vec!["checkout", "--orphan=plak/new", "--"],
        vec!["switch", "plak/c2"],
        vec!["rebase", "--onto", head, head, "plak/c2"],
        vec!["rebase", "--onto", head, head, "plak/c2", "--empty", "drop"],
        vec![
            "rebase",
            "--onto",
            head,
            head,
            "plak/c2",
            "--whitespace",
            "fix",
        ],
        vec!["rebase", "--update-refs", head],
        vec!["rebase", "--update-r", head],
        vec!["update-ref", "--std"],
        vec!["push", ".", "--dele", "refs/plak/pin"],
        vec!["branch", "--del", "feature", "plak/c2"],
        vec!["branch", "--mov", "--force", "plak/c1", "plak/c2"],
        vec!["branch", "--color", "-D", "feature", "plak/c2"],
        vec!["switch", "--cr", "plak/new"],
        vec!["checkout", "--orph", "plak/new"],
        vec!["checkout", "-qb", "plak/new"],
        vec!["switch", "-qC", "plak/c2"],
        vec!["checkout", "-q", "plak/c2", "--"],
        vec![
            "checkout",
            "-q",
            "plak/c2",
            "--pathspec-from-file=/dev/null",
        ],
        vec!["checkout", "-q", "plak/c2", empty_pathspec_option.as_str()],
        vec![
            "checkout",
            "-q",
            "plak/c2",
            "--pathspec-from-file",
            "empty-pathspec",
        ],
        vec!["checkout", "-q", "plak/c2", "--pathspec-from-file=-"],
        vec!["-c", "rebase.updateRefs=true", "rebase", "HEAD"],
        vec![
            "-c",
            "rebase.updateRefs=true",
            "pull",
            "-q",
            "--rebase",
            "origin",
            "up",
        ],
        vec![
            "-c",
            "rebase.updateRefs=true",
            "pull",
            "--rebase=merges",
            "origin",
            "up",
        ],
        vec![
            "-c",
            "rebase.updateRefs=true",
            "pull",
            "-qr",
            "origin",
            "up",
        ],
        vec![
            "-c",
            "rebase.updateRefs=true",
            "pull",
            "--no-rebase",
            "--rebase",
            "origin",
            "up",
        ],
        vec![
            "-c",
            "rebase.updateRefs=true",
            "-c",
            "pull.rebase=true",
            "pull",
            "origin",
            "up",
        ],
        vec![
            "-c",
            "rebase.updateRefs=true",
            "-c",
            "pull.rebase",
            "pull",
            "origin",
            "up",
        ],
        vec![
            "-c",
            "rebase.updateRefs=true",
            "-c",
            "branch.plak/c1.rebase=merges",
            "pull",
            "origin",
            "up",
        ],
        vec!["send-pack", ".", ":refs/plak/pin"],
        vec!["send-pack", parent_path.as_str(), ":refs/plak/pin"],
        vec!["send-pack", ".", "refs/plak/pin"],
        vec!["send-pack", ".", "--all"],
        vec!["send-pack", ".", "--mirror"],
        vec!["send-pack", "--mirror", "."],
        vec!["-c", "remote.self.mirror=true", "send-pack", "self"],
        vec!["send-pack", ".", "--stdin"],
        vec!["push", "self", ":refs/plak/pin"],
        vec!["-c", "remote.me.url=.", "push", "me", ":refs/plak/pin"],
        vec![
            "-c",
            "remote.me.url=elsewhere",
            "-c",
            "remote.me.pushurl=.",
            "push",
            "me",
            ":refs/plak/pin",
        ],
        vec![
            "-c",
            "remote.mirror.url=.",
            "-c",
            "remote.mirror.fetch=+refs/*:refs/*",
            "fetch",
            "--prune",
            "mirror",
        ],
        vec!["push", "local-copy:", ":refs/plak/pin"],
        vec!["push", "push-rewrite-self", ":refs/plak/pin"],
        vec!["reflog", "drop", "refs/stash"],
        vec!["-c", "gc.pruneExpire=now", "gc"],
        vec!["-c", "gc.reflogExpire=now", "gc"],
        vec![
            "-c",
            "gc.reflogExpireUnreachable=now",
            "maintenance",
            "run",
            "--task=gc",
        ],
        vec!["repack", "-a", "-d"],
        vec!["repack", "-ad"],
    ]
    .into_iter()
    .map(|args| args.into_iter().map(str::to_owned).collect())
    .collect();
    // Every guard must start after the parsed verb, not a same-named global value.
    for args in refused
        .clone()
        .into_iter()
        .filter(|args| !args[0].starts_with('-'))
    {
        let mut globals = vec!["--namespace".to_owned(), args[0].clone()];
        globals.extend(args);
        refused.push(globals);
    }
    for args in &refused {
        let output = output_retrying(Command::new(&shim).args(args).current_dir(&child));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(128), "{args:?}: {stderr}");
        assert!(
            stderr.contains("plak: git-guard refused:"),
            "{args:?}: {stderr}"
        );
        if args.iter().any(|arg| arg == "send-pack")
            && args
                .iter()
                .any(|arg| arg == "--mirror" || arg.ends_with(".mirror=true"))
        {
            assert!(
                stderr.contains("send-pack would change"),
                "{args:?}: {stderr}"
            );
        }
        assert_eq!(coordination_refs(parent.path()), refs_before, "{args:?}");
        assert_eq!(
            std::fs::read(parent.path().join(".git/index")).unwrap(),
            index_before,
            "{args:?}"
        );
    }
}

/// Ordinary history, listing, and a push to a real remote stay admitted.
#[test]
fn guard_admits_ordinary_git() {
    let parent = git_repo();
    git_ok(
        parent.path(),
        &["config", "user.email", "plak@example.test"],
    );
    git_ok(parent.path(), &["config", "user.name", "plak"]);
    std::fs::write(parent.path().join("f"), "file\n").unwrap();
    git_ok(parent.path(), &["add", "f"]);
    git_ok(parent.path(), &["commit", "-qm", "file"]);
    git_ok(parent.path(), &["branch", "plak/c2"]);
    let child = mint_child(parent.path(), "c1");
    let bare = parent.path().join("origin.git");
    git_ok(
        parent.path(),
        &["init", "-q", "--bare", bare.to_str().unwrap()],
    );
    git_ok(&child, &["remote", "add", "origin", bare.to_str().unwrap()]);
    std::fs::create_dir(child.join("update-ref")).unwrap();
    git_ok(&child, &["push", "origin", "HEAD:main"]);
    git_ok(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let scp = "git@example.invalid:plak/plak.git";
    let scp_upper = "git@example.invalid:Plak/plak.git";
    let remote_pushurl = format!("remote.command-only.pushurl={}", bare.display());
    let pathspecs = parent.path().join("checkout-paths");
    std::fs::write(&pathspecs, "f\n").unwrap();
    let rewrite = format!("url.{}.insteadOf", bare.display());
    git_ok(&child, &["config", "--add", &rewrite, scp]);
    git_ok(&child, &["config", "--add", &rewrite, scp_upper]);
    git_ok(&child, &["fetch", "origin"]);
    git_ok(
        &child,
        &["branch", "--set-upstream-to=origin/main", "plak/c1"],
    );
    git_ok(
        &child,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    let shim = install(&child).unwrap();
    for args in [
        &["commit", "--allow-empty", "-qm", "empty"][..],
        &["reset", "--hard"],
        &["branch", "--list", "plak/*"],
        &["branch", "-v", "--list", "plak/*"],
        &["--namespace", "branch", "branch", "--list", "plak/*"],
        &[
            "--work-tree",
            "update-ref",
            "update-ref",
            "refs/heads/global-option-feature",
            "HEAD",
        ],
        &["--attr-source", "HEAD", "branch", "--list", "plak/*"],
        &["branch", "--merged", "plak/c2"],
        &["branch", "--no-merged"],
        &["branch", "--merged=plak/c2", "plak/*"],
        &["branch", "--no-merged=plak/c2", "plak/*"],
        &["branch", "--contains=plak/c2", "plak/*"],
        &["branch", "--points-at=plak/c2", "plak/*"],
        &["branch", "feature", "plak/c2"],
        &["branch", "-f", "feature", "plak/c2"],
        &["branch", "-v", "-f", "feature", "plak/c2"],
        &["update-ref", "refs/heads/plak/c1", "HEAD"],
        &[
            "symbolic-ref",
            "-m",
            "refs/plak/readable-message",
            "refs/heads/alias",
            "refs/heads/feature",
        ],
        &["symbolic-ref", "refs/heads/alias"],
        &[
            "symbolic-ref",
            "-m",
            "--del",
            "refs/heads/alias",
            "refs/heads/feature",
        ],
        &["checkout", "plak/c2", "--", "f"],
        &[
            "checkout",
            "plak/c2",
            "--pathspec-from-file",
            pathspecs.to_str().unwrap(),
        ],
        &["stash", "list"],
        &["push", "origin", "HEAD:feature"],
        &["push", "origin", "HEAD:refs/plak/remote-pin"],
        &[
            "send-pack",
            bare.to_str().unwrap(),
            "HEAD:refs/heads/send-pack-feature",
        ],
        &[
            "-c",
            "remote.command-only.url=.",
            "-c",
            &remote_pushurl,
            "push",
            "command-only",
            "HEAD:refs/plak/command-line-remote-pin",
        ],
        &["push", "--repo=.", "origin", "HEAD:remote-repo-precedence"],
        &[
            "-c",
            "remote.remote-copy.url=origin.git",
            "status",
            "--porcelain",
        ],
        &["fetch", scp, "main"],
        &["pull", "--ff-only", scp_upper],
        &["fetch", "--multiple", "origin"],
        &["fetch", "--all"],
        &["fetch", "--prune"],
        &["pull", "--ff-only", "--prune"],
        &[
            "-c",
            "rebase.updateRefs=true",
            "pull",
            "--rebase",
            "--no-rebase",
            "origin",
            "main",
        ],
        &[
            "-c",
            "rebase.updateRefs=true",
            "pull",
            "--rebase=false",
            "origin",
            "main",
        ],
        &[
            "-c",
            "rebase.updateRefs=true",
            "-c",
            "pull.rebase=false",
            "pull",
            "origin",
            "main",
        ],
        &[
            "-c",
            "rebase.updateRefs=true",
            "-c",
            "pull.rebase=true",
            "-c",
            "branch.plak/c1.rebase=false",
            "pull",
            "origin",
            "main",
        ],
        &["remote", "update", "--prune"],
        &["remote", "prune", "origin"],
        &["-c", "remotes.team=origin", "remote", "update", "team"],
        &["-c", "remotes.default=origin", "remote", "update"],
        &[
            "-c",
            "fetch.prune=true",
            "-c",
            "remote.origin.prune=false",
            "-c",
            "remote.origin.fetch=+refs/nothing/*:refs/tags/*",
            "fetch",
        ],
        &[
            "-c",
            "fetch.prune=true",
            "-c",
            "remote.origin.fetch=+refs/nothing/*:refs/tags/*",
            "fetch",
            "--no-prune",
        ],
        &[
            "-c",
            "remote.origin.fetch=+refs/nothing/*:refs/*",
            "remote",
            "prune",
            "--dry-run",
            "origin",
        ],
        &[
            "fetch",
            "--prune",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
        &[
            "worktree",
            "lock",
            "--reason",
            "this is a reason, not a path",
            child.to_str().unwrap(),
        ],
        &["worktree", "unlock", child.to_str().unwrap()],
        &["worktree", "prune", "--dry-run"],
        &["-c", "gc.pruneExpire=now", "status", "--porcelain"],
        &["rebase", "--empty", "drop", "HEAD"],
        &["rebase", "--whitespace", "fix", "HEAD"],
        &["rebase", "--no-update-refs", "HEAD"],
        &[
            "-c",
            "rebase.updateRefs=true",
            "rebase",
            "--no-update-refs",
            "HEAD",
        ],
        &["gc"],
        &["prune", "-n"],
        &["checkout", "--detach"],
        &["branch", "-f", "plak/c1"],
    ] {
        let output = output_retrying(Command::new(&shim).args(args).current_dir(&child));
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Older supported platform Git lacks this global, but admission must
        // still pass it through rather than misidentifying HEAD as the verb.
        if args.first() == Some(&"--attr-source")
            && stderr.contains("unknown option: --attr-source")
        {
            assert!(!stderr.contains("git-guard refused"));
            continue;
        }
        assert!(output.status.success(), "{args:?}: {stderr}");
        assert!(!stderr.contains("git-guard refused"), "{args:?}: {stderr}");
    }
}

#[test]
fn configured_fetches_guard_default_remotes_groups_and_effective_pruning() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    git_ok(
        &child,
        &["remote", "add", "origin", parent.path().to_str().unwrap()],
    );
    git_ok(
        &child,
        &["remote", "add", "other", parent.path().to_str().unwrap()],
    );
    git_ok(&child, &["update-ref", "refs/plak/pin", "HEAD"]);
    git_ok(&child, &["branch", "plak/orphan"]);
    let shim = install(&child).unwrap();
    let refs_before = coordination_refs(parent.path());
    let index_before = std::fs::read(parent.path().join(".git/index")).unwrap();
    for (settings, args) in [
        (
            vec![("remote.origin.fetch", "+refs/nothing/*:refs/*")],
            vec!["fetch", "--prune"],
        ),
        (
            vec![("remote.origin.fetch", "+refs/nothing/*:refs/*")],
            vec!["pull", "--prune"],
        ),
        (
            vec![("remote.origin.fetch", "+refs/nothing/*:refs/*")],
            vec!["remote", "update", "--prune"],
        ),
        (
            vec![("remote.origin.fetch", "+refs/nothing/*:refs/*")],
            vec!["remote", "prune", "origin"],
        ),
        (
            vec![("remote.origin.fetch", "+refs/heads/*:refs/heads/plak/*")],
            vec!["fetch"],
        ),
        (
            vec![
                ("remote.origin.fetch", "+refs/nothing/*:refs/tags/*"),
                ("fetch.prune", "true"),
            ],
            vec!["fetch"],
        ),
        (
            vec![
                ("remote.origin.fetch", "+refs/nothing/*:refs/tags/*"),
                ("remote.origin.prune", "true"),
            ],
            vec!["pull"],
        ),
        (
            vec![
                ("remote.origin.fetch", "+refs/nothing/*:refs/tags/*"),
                ("fetch.prune", "true"),
            ],
            vec!["remote", "update"],
        ),
        (
            vec![("remote.origin.fetch", "+refs/nothing/*:refs/tags/*")],
            vec!["remote", "prune", "origin"],
        ),
        (
            vec![],
            vec![
                "-c",
                "remote.origin.fetch=+refs/nothing/*:refs/*",
                "fetch",
                "--prune",
            ],
        ),
        (
            vec![],
            vec![
                "-c",
                "remote.origin.fetch=+refs/heads/*:refs/heads/plak/*",
                "fetch",
            ],
        ),
        (
            vec![
                ("remote.other.fetch", "+refs/nothing/*:refs/*"),
                ("branch.plak/c1.remote", "other"),
            ],
            vec!["fetch", "--prune"],
        ),
        (
            vec![
                ("remote.other.fetch", "+refs/nothing/*:refs/*"),
                ("remotes.team", "other"),
            ],
            vec!["remote", "update", "team"],
        ),
        (
            vec![
                ("remote.other.fetch", "+refs/nothing/*:refs/*"),
                ("remotes.default", "other"),
            ],
            vec!["remote", "update"],
        ),
        (
            vec![
                ("remote.other.fetch", "+refs/nothing/*:refs/*"),
                ("remotes.team", "other"),
            ],
            vec!["fetch", "--multiple", "team"],
        ),
        (vec![("rebase.updateRefs", "true")], vec!["rebase", "HEAD"]),
        (
            vec![("rebase.updateRefs", "true"), ("pull.rebase", "true")],
            vec!["pull", "origin", "up"],
        ),
        (
            vec![
                ("rebase.updateRefs", "true"),
                ("branch.plak/c1.rebase", "merges"),
            ],
            vec!["pull", "origin", "up"],
        ),
    ] {
        for remote in ["origin", "other"] {
            git_ok(
                &child,
                &[
                    "config",
                    "--replace-all",
                    &format!("remote.{remote}.fetch"),
                    &format!("+refs/heads/*:refs/remotes/{remote}/*"),
                ],
            );
        }
        for (key, value) in &settings {
            git_ok(&child, &["config", "--replace-all", key, value]);
        }
        let output = output_retrying(Command::new(&shim).args(&args).current_dir(&child));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(128),
            "{settings:?} {args:?}: {stderr}"
        );
        assert!(
            stderr.contains("git-guard refused"),
            "{settings:?} {args:?}: {stderr}"
        );
        assert_eq!(
            coordination_refs(parent.path()),
            refs_before,
            "{settings:?} {args:?}"
        );
        assert_eq!(
            std::fs::read(parent.path().join(".git/index")).unwrap(),
            index_before,
            "{settings:?} {args:?}"
        );
        for (key, _) in &settings {
            git_ok(&child, &["config", "--unset-all", key]);
        }
    }
}

/// A stdin update of a pin is refused. An admitted stdin script still reaches git.
#[test]
fn update_ref_stdin_naming_a_pin_is_refused_and_stdin_is_preserved_when_admitted() {
    use std::io::Write as _;
    let parent = git_repo();
    git_ok(
        parent.path(),
        &["config", "user.email", "plak@example.test"],
    );
    git_ok(parent.path(), &["config", "user.name", "plak"]);
    let child = mint_child(parent.path(), "c1");
    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(parent.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let head = head.trim().to_owned();
    git_ok(
        parent.path(),
        &["update-ref", "refs/plak/pin", head.as_str()],
    );
    let shim = install(&child).unwrap();
    let run_stdin_with_namespace = |script: &str, zero: bool, namespace: Option<&str>| {
        let mut command = Command::new(&shim);
        if let Some(namespace) = namespace {
            command.args(["--namespace", namespace]);
        }
        command.args(["update-ref", "--stdin"]);
        if zero {
            command.arg("-z");
        }
        let mut process = command
            .current_dir(&child)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        process
            .stdin
            .as_mut()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        process.wait_with_output().unwrap()
    };
    let run_stdin = |script: &str, zero: bool| run_stdin_with_namespace(script, zero, None);
    let refused = run_stdin_with_namespace(
        "symref-create refs/heads/alias refs/heads/unoccupied\ndelete refs/plak/pin\n",
        false,
        Some("-z"),
    );
    assert_eq!(
        refused.status.code(),
        Some(128),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("git-guard refused"));
    let refused = run_stdin(&format!("update refs/plak/pin {head}\n"), false);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(128), "{stderr}");
    assert!(stderr.contains("refs/plak/pin"), "{stderr}");
    let pin = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "refs/plak/pin"])
            .current_dir(parent.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(pin.trim(), head);
    let before = coordination_refs(parent.path());
    let checked_out_target = format!(
        "symref-create refs/heads/alias {}\n",
        git_stdout(parent.path(), &["symbolic-ref", "HEAD"]).trim()
    );
    let checked_out_target_nul = format!(
        "symref-create refs/heads/alias\0{}\0",
        git_stdout(parent.path(), &["symbolic-ref", "HEAD"]).trim()
    );
    for (script, zero) in [
        ("symref-create refs/heads/alias refs/plak/pin\n", false),
        ("symref-update refs/heads/alias refs/plak/pin\n", false),
        ("symref-delete refs/plak/pin\n", false),
        ("symref-verify refs/plak/pin\n", false),
        ("symref-create refs/heads/alias\0refs/plak/pin\0", true),
        ("symref-update refs/heads/alias\0refs/plak/pin\0\0", true),
        ("symref-delete refs/plak/pin\0\0", true),
        ("symref-verify refs/plak/pin\0\0", true),
        ("update main-worktree/HEAD HEAD\n", false),
        ("update worktrees/c2/HEAD HEAD\n", false),
        ("update main-worktree/HEAD\0HEAD\0\0", true),
        (checked_out_target.as_str(), false),
        (checked_out_target_nul.as_str(), true),
        ("delete \"refs/plak/pin\"\n", false),
    ] {
        let refused = run_stdin(script, zero);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert_eq!(refused.status.code(), Some(128), "{script:?}: {stderr}");
        assert!(stderr.contains("git-guard refused"), "{script:?}: {stderr}");
        assert_eq!(coordination_refs(parent.path()), before);
    }
    let admitted = run_stdin(&format!("update refs/heads/feature {head}\n"), false);
    assert!(
        admitted.status.success(),
        "{}",
        String::from_utf8_lossy(&admitted.stderr)
    );
    let feature = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "refs/heads/feature"])
            .current_dir(parent.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(feature.trim(), head);
    let admitted = run_stdin(&format!("update refs/heads/nul-feature\0{head}\0\0"), true);
    assert!(
        admitted.status.success(),
        "{}",
        String::from_utf8_lossy(&admitted.stderr)
    );
    assert_eq!(
        git_stdout(parent.path(), &["rev-parse", "refs/heads/nul-feature"]).trim(),
        head
    );
}

#[test]
fn stdin_transaction_lists_worktrees_once_and_preserves_all_updates() {
    use std::io::Write as _;
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let scratch = tempfile::tempdir().unwrap();
    let calls = scratch.path().join("worktree-calls");
    let wrapper = scratch.path().join("real-git");
    let real_git = super::discover_real_git().unwrap();
    super::write_executable(&wrapper, &format!(
        "#!/bin/sh\nif [ \"$3\" = worktree ] && [ \"$4\" = list ]; then printf x >> {}; fi\nexec {} \"$@\"\n",
        super::sh_single_quote(calls.to_str().unwrap()), super::sh_single_quote(real_git.to_str().unwrap())
    )).unwrap();
    let shim = scratch.path().join("git");
    super::write_executable(&shim, &super::shim_script(&child, &wrapper, "plak/c1")).unwrap();
    let mut process = Command::new("/bin/sh")
        .arg(&shim)
        .args(["update-ref", "--stdin"])
        .current_dir(&child)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"update refs/heads/first HEAD\nupdate refs/heads/second HEAD\n")
        .unwrap();
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(calls).unwrap(), "x");
    let first = git_stdout(&child, &["rev-parse", "refs/heads/first"]);
    assert_eq!(
        first,
        git_stdout(&child, &["rev-parse", "refs/heads/second"])
    );
}

#[test]
fn global_admission_and_retention_config_are_checked_before_dispatch() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let shim = install(&child).unwrap();
    for (args, parameters, marker) in [
        (
            vec!["--attr-source", "HEAD", "init"],
            None,
            "git-redirect refused",
        ),
        (
            vec!["--attr-source", "HEAD", "worktree", "add", "other"],
            None,
            "git-redirect refused",
        ),
        (
            vec!["--config-env=core.worktree=UNSET_GIT_WORKTREE", "status"],
            None,
            "git-redirect refused",
        ),
        (
            vec!["gc"],
            Some("'gc.pruneExpire=now'"),
            "git-guard refused",
        ),
        (
            vec!["gc"],
            Some("'gc.pruneExpire'='now'"),
            "git-guard refused",
        ),
        (
            vec!["maintenance", "run", "--task=gc"],
            Some("'gc.reflogExpire=now'"),
            "git-guard refused",
        ),
    ] {
        let mut command = Command::new(&shim);
        command.args(&args).current_dir(&child);
        if let Some(value) = parameters {
            command.env("GIT_CONFIG_PARAMETERS", value);
        }
        let output = output_retrying(&mut command);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(128), "{args:?}: {stderr}");
        assert!(stderr.contains(marker), "{args:?}: {stderr}");
    }
    for (key, value, args, marker) in [
        ("gc.pruneExpire", "now", vec!["gc"], "git-guard refused"),
        (
            "gc.reflogExpire",
            "now",
            vec!["maintenance", "run", "--task=gc"],
            "git-guard refused",
        ),
        (
            "core.worktree",
            "..",
            vec!["status"],
            "git-redirect refused",
        ),
    ] {
        let output = output_retrying(
            Command::new(&shim)
                .args(&args)
                .current_dir(&child)
                .env("GIT_CONFIG_COUNT", "2")
                .env("GIT_CONFIG_KEY_0", "advice.detachedHead")
                .env("GIT_CONFIG_VALUE_0", "false")
                .env("GIT_CONFIG_KEY_1", key)
                .env("GIT_CONFIG_VALUE_1", value),
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(128), "{key}: {stderr}");
        assert!(stderr.contains(marker), "{key}: {stderr}");
    }
    for args in [
        vec!["status", "--porcelain"],
        vec!["-c", "gc.reflogExpire=now", "status", "--porcelain"],
    ] {
        let output = output_retrying(Command::new(&shim).args(&args).current_dir(&child));
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Switching the child off its fork branch does not change the own branch the
/// shim was minted with, so reclamation still recognizes that shim.
#[test]
fn reclamation_accepts_the_guarded_shim_after_the_child_switches_branch() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let exclude = parent.path().join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(&exclude, ".plak/bin/\n").unwrap();
    install(&child).unwrap();
    git_ok(&child, &["switch", "-c", "feature"]);
    assert!(crate::occupancy_vcs::maintenance::check_reclaimable(&child, None).is_ok());
    git_ok(&child, &["checkout", "--detach"]);
    assert!(crate::occupancy_vcs::maintenance::check_reclaimable(&child, None).is_ok());
}

/// A detached snapshot owns no `plak/<id>` branch, so that name stays protected.
#[test]
fn detached_snapshot_owns_no_branch() {
    let parent = git_repo();
    let child = parent.path().join(".plak/worktrees/snap");
    std::fs::create_dir_all(child.parent().unwrap()).unwrap();
    git_ok(
        parent.path(),
        &["worktree", "add", "-q", "--detach", child.to_str().unwrap()],
    );
    let shim = install(&child).unwrap();
    let output = output_retrying(
        Command::new(&shim)
            .args(["branch", "plak/snap"])
            .current_dir(&child),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(128), "{stderr}");
    assert!(
        stderr.contains("git-guard refused") && stderr.contains("plak/snap"),
        "{stderr}"
    );
    let refused = output_retrying(Command::new(&shim).arg("stash").current_dir(&child));
    assert_eq!(refused.status.code(), Some(128));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("git stash create"));
}

/// Reclamation treats the regenerated guarded shim as the managed fence.
#[test]
fn reclamation_accepts_the_regenerated_guarded_shim() {
    let parent = git_repo();
    let child = mint_child(parent.path(), "c1");
    let exclude = parent.path().join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(&exclude, ".plak/bin/\n").unwrap();
    install(&child).unwrap();
    assert!(crate::occupancy_vcs::maintenance::check_reclaimable(&child, None).is_ok());
    std::fs::write(child.join(".plak/bin/git"), "#!/bin/sh\nexit 0\n").unwrap();
    assert!(crate::occupancy_vcs::maintenance::check_reclaimable(&child, None).is_err());
}
