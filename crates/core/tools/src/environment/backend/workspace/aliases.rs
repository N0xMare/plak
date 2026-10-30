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
mod tests;
