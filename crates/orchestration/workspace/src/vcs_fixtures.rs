//! Real-VCS fixtures for the `agent_tree` unit tests: every helper here
//! shells to `git` or `jj`. Only tests that are explicitly VCS-backed (a
//! candidate seal, an adopt landing, a mint hook in a real copy) import this
//! module; everything else runs on `FakeVcs` and a plain temp dir.

use std::path::Path;

pub use std::process::Command;
use tempfile::tempdir;

/// A seeded git repository on `main` with one commit and a local identity.
pub fn git_repo() -> tempfile::TempDir {
    let root = tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "plak@example.test"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "plak"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "SEED.txt"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    root
}

/// Stages `paths` and commits them with `message`.
pub fn git_commit(cwd: &Path, paths: &[&str], message: &str) {
    let mut args = vec!["add"];
    args.extend(paths.iter().copied());
    assert!(
        Command::new("git")
            .args(&args)
            .current_dir(cwd)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(cwd)
            .status()
            .unwrap()
            .success()
    );
}

/// Whether a usable `jj` is on this machine; VCS-backed jj tests skip without one.
pub fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Runs a jj command in `path` the way a human at the parent checkout
/// would: enough to stale every other workspace in the repo.
/// Runs `jj` in `path` and asserts it succeeded.
pub fn jj_in(path: &Path, args: &[&str]) {
    assert!(
        Command::new("jj")
            .args(["--no-pager"])
            .args([
                "--config",
                "user.name=plak",
                "--config",
                "user.email=plak@local"
            ])
            .args(args)
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
}

/// A seeded colocated jj repository with one commit.
pub fn jj_repo() -> tempfile::TempDir {
    let root = tempdir().unwrap();
    assert!(
        Command::new("jj")
            .args(["--no-pager", "git", "init"])
            .args([
                "--config",
                "user.name=plak",
                "--config",
                "user.email=plak@local"
            ])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(root.path())
            .status()
            .unwrap()
            .success(),
        "jj git init failed"
    );
    std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
    assert!(
        Command::new("jj")
            .args(["--no-pager", "commit", "-m", "seed"])
            .args([
                "--config",
                "user.name=plak",
                "--config",
                "user.email=plak@local"
            ])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(root.path())
            .status()
            .unwrap()
            .success(),
        "jj seed commit failed"
    );
    root
}

/// Describes and commits `paths` in a jj working copy.
pub fn jj_commit(cwd: &Path, paths: &[&str], message: &str) {
    for path in paths {
        assert!(
            Command::new("jj")
                .args(["--no-pager", "file", "track", path])
                .args([
                    "--config",
                    "user.name=plak",
                    "--config",
                    "user.email=plak@local"
                ])
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(cwd)
                .status()
                .unwrap()
                .success()
        );
    }
    assert!(
        Command::new("jj")
            .args(["--no-pager", "commit", "-m", message])
            .args([
                "--config",
                "user.name=plak",
                "--config",
                "user.email=plak@local"
            ])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(cwd)
            .status()
            .unwrap()
            .success()
    );
}

/// The repository's current `HEAD` commit id.
pub fn git_head(cwd: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// `git status --porcelain` for the copy, verbatim.
pub fn git_porcelain(cwd: &Path) -> String {
    let output = Command::new("git")
        .args(["status", "--porcelain", "-uall"])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Every path recorded in `tree_id`, sorted.
pub fn git_tree_paths(cwd: &Path, tree_id: &str) -> Vec<String> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", tree_id])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}
