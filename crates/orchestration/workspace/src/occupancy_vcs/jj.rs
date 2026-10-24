//! Native JJ snapshot, adopt, and workspace queries.

use super::{AdoptFailure, AdoptLanding, Path, PathBuf, VcsError, VcsSnapshot, mint_error};

/// A jj copy that is no longer a workspace of the launch repo (live
/// 2026-09-08 D-J1).
///
/// `jj workspace add` writes `{copy}/.jj/repo` as a **pointer file** naming
/// the launch repo. A standalone repo — `jj git init` run inside the copy,
/// with the pointer moved aside — writes a `.jj/repo` **directory** instead.
/// Such a copy still answers `jj status` and `jj log`, so nothing downstream
/// notices: its seal compares a foreign change against the recorded base
/// (`Revision ... doesn't exist`, or a bogus "diverged") and its adopt would
/// rebase a range the launch repo has never heard of. Only minted copies are
/// asked; the launch checkout's own `.jj/repo` is a directory by definition.
pub(super) fn jj_detached_copy(path: &Path) -> Option<PathBuf> {
    if !crate::git_redirect_fence::is_minted_occupancy_copy(path) {
        return None;
    }
    let pointer = path.join(".jj").join("repo");
    pointer.is_dir().then_some(pointer)
}

/// Typed refusal for a copy that stopped being a workspace of the launch repo.
fn jj_require_workspace(path: &Path) -> Result<(), VcsError> {
    match jj_detached_copy(path) {
        Some(pointer) => Err(mint_error(&format!(
            "the copy is not a workspace of the launch repo: `{}` is a standalone jj repo",
            pointer.display()
        ))),
        None => Ok(()),
    }
}

/// Whether the workspace has the immutable candidate as its direct parent.
pub(crate) fn jj_copy_is_over_candidate(
    launch: &Path,
    agent_id: &str,
    candidate: &VcsSnapshot,
) -> bool {
    crate::native_jj::over(launch, agent_id, candidate)
}

/// Serializes concurrent workspace creation by this process per launch repository.
pub(super) fn jj_mint_lock(launch: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(map.entry(launch.to_path_buf()).or_default())
}

pub(super) fn jj_snapshot(path: &Path) -> Result<VcsSnapshot, VcsError> {
    jj_require_workspace(path)?;
    crate::native_jj::snapshot(path)
}

/// Compare the current native tree against the immutable minted base tree.
pub(crate) fn jj_diverged_from_base(
    path: &Path,
    base_change_id: Option<&str>,
    base_commit: &str,
) -> Result<bool, VcsError> {
    let _ = base_change_id;
    jj_require_workspace(path)?;
    crate::native_jj::diverged(path, base_commit)
}

pub(super) fn jj_adopt(
    target_path: &Path,
    child_path: &Path,
    base_commit: &str,
    base_change_id: Option<&str>,
    candidate: &VcsSnapshot,
    expected_target: Option<&str>,
) -> Result<AdoptLanding, AdoptFailure> {
    let _ = base_change_id;
    jj_require_workspace(child_path).map_err(|e| AdoptFailure::Unavailable(e.detail))?;
    crate::native_jj::adopt(
        target_path,
        child_path,
        base_commit,
        candidate,
        expected_target,
    )
}

pub(super) fn jj_workspace_dirty(path: &Path) -> bool {
    crate::native_jj::dirty(path)
}

pub(super) fn jj_ahead_behind(
    child: &Path,
    parent: &Path,
    base: Option<&str>,
) -> Option<(u64, u64)> {
    crate::native_jj::ahead_behind(child, parent, base)
}

/// Refuses a JJ copy whose recorded mint base is no longer its strict ancestor.
pub fn validate_jj_copy_base(path: &Path, base: &str) -> Result<(), VcsError> {
    crate::native_jj::validate_copy_base(path, base)
}
