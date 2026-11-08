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

pub(super) fn read_session(session: &plakous_hosted::runtime::SurfaceSession) -> WorkspaceChrome {
    if session.runner.is_none() {
        return read(&session.workspace);
    }
    WorkspaceChrome {
        path: session.workspace.clone(),
        home: None,
        branch: None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_remote_path_present_on_the_host_never_reads_host_vcs_or_home() {
        if !cfg!(feature = "git-cli") {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-b", "host-only"])
                .current_dir(root.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.test",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "fixture"
                ])
                .current_dir(root.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(
            super::read(root.path()).branch.as_deref(),
            Some("host-only")
        );
        let session = plakous_hosted::runtime::SurfaceSession {
            runner: Some(plakous_hosted::sessions::RunnerDisplay {
                name: "peer".into(),
                runner_id: "peer-id".into(),
                workspace_root: root.path().to_path_buf(),
                workspace_cwd: root.path().to_path_buf(),
            }),
            id: "fixture".into(),
            account: None,
            model: None,
            effort: None,
            fast: false,
            workspace: root.path().to_owned(),
            roster: None,
            live_children: false,
            mcp: None,
        };
        let shown = super::read_session(&session);
        assert_eq!(shown.path, root.path());
        assert_eq!(shown.branch, None);
        assert_eq!(shown.home, None);
    }

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
