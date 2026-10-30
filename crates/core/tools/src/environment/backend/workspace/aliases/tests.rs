use super::super::mutate_workspace_sync;
#[cfg(target_os = "macos")]
use super::ExecutionBackendErrorKind;
use super::{ExecutionWorkspace, fs};
use crate::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    ExecutionOperationControl, WorkspaceContentIdentity, WorkspaceExpectedFileState,
    WorkspaceMutation, WorkspaceMutationDisposition, WorkspaceMutationTransaction,
    WorkspaceReplacement,
};
use plakous_model::CancellationToken;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn workspace(root: &Path) -> Result<ExecutionWorkspace, Box<dyn std::error::Error>> {
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000997".parse()?),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    Ok(ExecutionWorkspace::from_environment(&environment))
}

fn control() -> ExecutionOperationControl {
    ExecutionOperationControl::new(CancellationToken::new())
}

#[test]
fn hardlinked_entries_can_get_independent_contents() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "old")?;
    fs::hard_link(root.path().join("a"), root.path().join("b"))?;
    let expected =
        WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(b"old")?);
    let transaction = WorkspaceMutationTransaction::new(vec![
        WorkspaceMutation::new(
            "a",
            expected,
            WorkspaceReplacement::content(b"first".to_vec()),
        )?,
        WorkspaceMutation::new(
            "b",
            expected,
            WorkspaceReplacement::content(b"second".to_vec()),
        )?,
    ])?;
    mutate_workspace_sync(&workspace(root.path())?, &transaction, &control())?;
    assert_eq!(fs::read(root.path().join("a"))?, b"first");
    assert_eq!(fs::read(root.path().join("b"))?, b"second");
    assert_ne!(
        fs::metadata(root.path().join("a"))?.ino(),
        fs::metadata(root.path().join("b"))?.ino()
    );
    assert_eq!(fs::read_dir(root.path())?.count(), 2);
    Ok(())
}

#[test]
fn unchanged_alias_parent_needs_no_write_access_or_probe() -> TestResult {
    let root = tempfile::tempdir()?;
    for name in ["a", "b"] {
        fs::write(root.path().join(name), "old")?;
    }
    let workspace = workspace(root.path())?;
    let expected =
        WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(b"old")?);
    let transaction = WorkspaceMutationTransaction::new(
        ["a", "b"]
            .into_iter()
            .map(|name| {
                WorkspaceMutation::new(
                    name,
                    expected,
                    WorkspaceReplacement::content(b"old".to_vec()),
                )
            })
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o555))?;
    let result = mutate_workspace_sync(&workspace, &transaction, &control());
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755))?;
    assert!(
        result?
            .items()
            .iter()
            .all(|item| item.disposition() == WorkspaceMutationDisposition::Unchanged)
    );
    assert_eq!(fs::read_dir(root.path())?.count(), 2);
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn existing_and_absent_case_aliases_are_conflicts() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "old")?;
    if !root.path().join("A").exists() {
        return Ok(());
    }
    let workspace = workspace(root.path())?;
    let expected =
        WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(b"old")?);
    for replacement in [
        WorkspaceReplacement::Missing,
        WorkspaceReplacement::content(b"x".to_vec()),
    ] {
        let transaction = WorkspaceMutationTransaction::new(vec![
            WorkspaceMutation::new("a", expected, replacement)?,
            WorkspaceMutation::new(
                "A",
                expected,
                WorkspaceReplacement::content(b"new".to_vec()),
            )?,
        ])?;
        let error = mutate_workspace_sync(&workspace, &transaction, &control())
            .err()
            .ok_or("case aliases accepted")?;
        assert_eq!(error.kind(), ExecutionBackendErrorKind::Conflict);
        assert_eq!(fs::read(root.path().join("a"))?, b"old");
    }
    let transaction = WorkspaceMutationTransaction::new(vec![
        WorkspaceMutation::new(
            "missing",
            WorkspaceExpectedFileState::Missing,
            WorkspaceReplacement::content(b"x".to_vec()),
        )?,
        WorkspaceMutation::new(
            "MISSING",
            WorkspaceExpectedFileState::Missing,
            WorkspaceReplacement::content(b"y".to_vec()),
        )?,
    ])?;
    let error = mutate_workspace_sync(&workspace, &transaction, &control())
        .err()
        .ok_or("absent case aliases accepted")?;
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Conflict);
    assert!(!root.path().join("missing").exists());
    assert_eq!(fs::read_dir(root.path())?.count(), 1);
    Ok(())
}
