//! Rejects mutation paths that the filesystem treats as the same target.

use rustix::io::Errno;
use std::collections::BTreeMap;

use super::{
    AtFlags, ExecutionBackendError, ExecutionBackendErrorKind, ExecutionWorkspace, Mode,
    NEXT_MUTATION_TEMP, OFlags, Ordering, PathBuf, PreflightMutation, WorkspaceMutationDisposition,
    conflict_error, fs, io_error, mkdirat, open_workspace_parent, openat, unlinkat,
};
use std::os::unix::fs::MetadataExt;

pub(super) fn check_path_aliases(
    workspace: &ExecutionWorkspace,
    preflight: &[PreflightMutation],
) -> Result<(), ExecutionBackendError> {
    let mut parents = BTreeMap::<(u64, u64), (fs::File, Vec<PathBuf>, bool)>::new();
    for item in preflight {
        let (parent, name) = match open_workspace_parent(workspace, &item.relative) {
            Ok(parent) => parent,
            // An unchanged absent path may still have no parent.
            Err(error) if error.kind() == ExecutionBackendErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let metadata = parent
            .metadata()
            .map_err(|_| io_error("mutation parent metadata failed"))?;
        let group = parents
            .entry((metadata.dev(), metadata.ino()))
            .or_insert_with(|| (parent, Vec::new(), false));
        group.1.push(name);
        group.2 |= item.disposition != WorkspaceMutationDisposition::Unchanged;
    }
    for (_, (parent, names, changed)) in parents {
        if names.len() < 2 || !changed {
            continue;
        }
        let mut probe = AliasProbe::new(parent)?;
        for name in names {
            probe.add(name)?;
        }
    }
    Ok(())
}

// Let the filesystem decide case folding and Unicode normalization, rather than
// approximating its rules in Rust. The probe has the target parent's filesystem
// semantics and never creates a requested target path.
struct AliasProbe {
    parent: fs::File,
    directory: fs::File,
    name: PathBuf,
    entries: Vec<PathBuf>,
}

impl AliasProbe {
    fn new(parent: fs::File) -> Result<Self, ExecutionBackendError> {
        for _ in 0..32 {
            let sequence = NEXT_MUTATION_TEMP.fetch_add(1, Ordering::Relaxed);
            let name = PathBuf::from(format!(".plak-alias-{}-{sequence}", std::process::id()));
            match mkdirat(&parent, &name, Mode::from_raw_mode(0o700)) {
                Ok(()) => {
                    let opened = openat(
                        &parent,
                        &name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    );
                    let directory = if let Ok(descriptor) = opened {
                        fs::File::from(descriptor)
                    } else {
                        let _ = unlinkat(&parent, &name, AtFlags::REMOVEDIR);
                        return Err(io_error("mutation alias probe open failed"));
                    };
                    return Ok(Self {
                        parent,
                        directory,
                        name,
                        entries: Vec::new(),
                    });
                }
                Err(Errno::EXIST) => {}
                Err(_) => return Err(io_error("mutation alias probe creation failed")),
            }
        }
        Err(io_error("mutation alias probe names exhausted"))
    }

    fn add(&mut self, name: PathBuf) -> Result<(), ExecutionBackendError> {
        match openat(
            &self.directory,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(_) => {
                self.entries.push(name);
                Ok(())
            }
            Err(Errno::EXIST) => Err(conflict_error(
                "workspace mutation paths alias on this filesystem",
            )),
            Err(_) => Err(io_error("mutation alias probe entry creation failed")),
        }
    }
}

impl Drop for AliasProbe {
    fn drop(&mut self) {
        for name in &self.entries {
            let _ = unlinkat(&self.directory, name, AtFlags::empty());
        }
        let _ = unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR);
    }
}

#[cfg(test)]
mod tests {
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
}
