//! Questions a host asks about workspace paths outside a tool operation.
//!
//! Tool operations reach the workspace through the sealed execution backend.
//! A host also asks a few questions of its own before and around them: the
//! physical path of a recorded root, whether a process shim is installed, the
//! size of a copy. [`WorkspaceProbe`] is that port, so the answer comes from
//! wherever the workspace lives; [`LocalWorkspaceProbe`] answers from this
//! process's filesystem.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Why a probe produced no answer at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProbeError {
    /// The probe could not answer: the host the workspace lives on is
    /// unreachable. Distinct from every "absent" answer, so a caller fails
    /// closed instead of acting on a path that only looks missing.
    Unavailable,
}

impl fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "the workspace probe is unavailable",
        })
    }
}

impl std::error::Error for ProbeError {}

/// Host-side questions about workspace paths, answered where the workspace
/// lives.
///
/// Every answer is fallible with [`ProbeError::Unavailable`], which is never an
/// "absent" answer. The port is synchronous in this step and becomes async in
/// a later step, together with the version-control port, once session
/// construction is async; until then a caller must not hold a lock across a
/// call.
pub trait WorkspaceProbe: Send + Sync {
    /// Physical object identity from the workspace's own filesystem.
    ///
    /// # Errors
    /// Unavailable probes fail closed, never substitute host metadata.
    fn filesystem_identity(
        &self,
        _path: &Path,
    ) -> Result<crate::HostFilesystemObjectIdentity, ProbeError> {
        Err(ProbeError::Unavailable)
    }
    /// The physical absolute path of `path`, as [`std::fs::canonicalize`]
    /// resolves it; `None` when it does not resolve (missing, or a component
    /// cannot be searched).
    ///
    /// # Errors
    /// Returns [`ProbeError::Unavailable`] when the probe cannot answer.
    fn canonicalize(&self, path: &Path) -> Result<Option<PathBuf>, ProbeError>;

    /// Whether `path` names a regular file, following links.
    ///
    /// # Errors
    /// Returns [`ProbeError::Unavailable`] when the probe cannot answer.
    fn is_file(&self, path: &Path) -> Result<bool, ProbeError>;

    /// Bytes of `path` and, for a directory, of every entry below it, without
    /// following links: a link counts its own length. `None` when an entry
    /// could not be inspected or listed.
    ///
    /// # Errors
    /// Returns [`ProbeError::Unavailable`] when the probe cannot answer.
    fn tree_bytes(&self, path: &Path) -> Result<Option<u64>, ProbeError>;
}

/// The probe of this process's own filesystem.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocalWorkspaceProbe;

impl WorkspaceProbe for LocalWorkspaceProbe {
    fn filesystem_identity(
        &self,
        path: &Path,
    ) -> Result<crate::HostFilesystemObjectIdentity, ProbeError> {
        crate::environment::filesystem_object_identity(path).map_err(|_| ProbeError::Unavailable)
    }
    fn canonicalize(&self, path: &Path) -> Result<Option<PathBuf>, ProbeError> {
        Ok(std::fs::canonicalize(path).ok())
    }

    fn is_file(&self, path: &Path) -> Result<bool, ProbeError> {
        Ok(path.is_file())
    }

    fn tree_bytes(&self, path: &Path) -> Result<Option<u64>, ProbeError> {
        Ok(directory_bytes(path).ok())
    }
}

fn directory_bytes(path: &Path) -> io::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut bytes = metadata.len();
    for entry in std::fs::read_dir(path)? {
        bytes = bytes.saturating_add(directory_bytes(&entry?.path())?);
    }
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
