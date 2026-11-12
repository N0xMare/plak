use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn inherited_git_steering_and_command_or_trace_injection_are_cleared() {
    const CHILD: &str = "PLAK_CHANGES_STEERING_FIXTURE";
    if let Some(root) = std::env::var_os(CHILD) {
        let environment = environment(Path::new(&root));
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let observed = status(&environment).await;
            assert_eq!(observed.repository.workspace, Path::new(&root));
            assert!(matches!(
                ask(&environment, diff_request(&observed, 0)).await,
                ChangesAnswer::Diff(ChangesDiff::Patch { .. })
            ));
        });
        return;
    }
    let root = checkout();
    let elsewhere = checkout();
    std::fs::write(root.path().join("file"), "changed\n").unwrap();
    let marker = root.path().join("marker");
    let payload = format!("sh -c 'touch {}'", marker.display());
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "environment::backend::changes::tests::unsupported::inherited_git_steering_and_command_or_trace_injection_are_cleared"])
        .env(CHILD, root.path()).env("GIT_DIR", elsewhere.path().join(".git"))
        .env("GIT_WORK_TREE", elsewhere.path()).env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.fsmonitor").env("GIT_CONFIG_VALUE_0", &payload)
        .env("GIT_EXTERNAL_DIFF", &payload).env("GIT_ATTR_SOURCE", "HEAD")
        .env("GIT_TRACE", &marker).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn old_git_and_a_build_ignoring_attribute_source_are_unavailable() {
    let root = tempfile::tempdir().unwrap();
    let programs = tempfile::tempdir().unwrap();
    for (name, behavior) in [
        (
            "old-git",
            "case \"$*\" in *--version*) printf 'git version 2.43.9\\n';; *) exit 99;; esac",
        ),
        (
            "ignored-feature",
            "case \"$*\" in *check-attr*) printf 'probe\\000changes-probe\\000set\\000';; *) exec git \"$@\";; esac",
        ),
        (
            "ignored-lazy-fetch",
            "unset GIT_NO_LAZY_FETCH; exec git \"$@\"",
        ),
    ] {
        let program = programs.path().join(name);
        std::fs::write(&program, format!("#!/bin/sh\n{behavior}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executor = process::Executor::new(
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            CancellationToken::new(),
            &std::env::var_os("PATH").unwrap(),
        )
        .test_program(program);
        assert_eq!(
            git::supported(&executor, root.path()).await,
            Err(ChangesRefusal::VcsUnavailable)
        );
    }
}

#[tokio::test]
async fn disabled_git_build_is_unavailable_before_repository_discovery() {
    let root = tempfile::tempdir().unwrap();
    let environment = environment(root.path());
    let answer = workspace_changes(
        &environment,
        ChangesRequest::status(),
        OperationContext::new(CancellationToken::new()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        answer,
        ChangesAnswer::Refused(ChangesRefusal::VcsUnavailable)
    );
}
