//! Read-only checkout facts, sampled outside the terminal thread.

use std::path::Path;

use crate::surface_protocol::WorkspaceChrome;

pub(super) fn read(workspace: &Path) -> WorkspaceChrome {
    WorkspaceChrome {
        path: workspace.to_path_buf(),
        home: crate::home::paths::user_home(),
        branch: plakous_hosted::workspace::workspace_branch(workspace),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn refreshed_chrome_observes_branch_changes_from_a_launch_subdirectory() {
        if !cfg!(feature = "git-cli") {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(root.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "before"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        let launch = root.path().join("nested");
        std::fs::create_dir(&launch).unwrap();
        assert_eq!(super::read(&launch).branch.as_deref(), Some("before"));
        git(&["checkout", "-b", "after"]);
        let view = super::read(&launch);
        assert_eq!(view.path, launch);
        assert_eq!(view.branch.as_deref(), Some("after"));
    }
}
