//! The [`VcsOps`] port and the production [`SystemVcs`] implementation.

use super::{
    AdoptFailure, AdoptLanding, BTreeMap, MintedCopy, Path, SessionVcs, VcsError, VcsSnapshot,
    WorkspaceMode, adopt_candidate, ahead_behind, copy_is_dirty, detect_vcs,
    jj_copy_is_over_candidate, jj_diverged_from_base, maintenance, mint_worktree_from,
    run_mint_hook, seal_candidate, unmint_copy, unpin_candidate, workspace_label,
};

/// The VCS port the tree drives. Every method is a
/// repository operation or a filesystem probe and is expected to run with the tree
/// mutex released; the tree itself never holds one of these across a seal,
/// an adopt, a hook, or an unmint.
pub trait VcsOps: Send + Sync {
    /// Proves normal sealing covers every non-disposable byte before reclaim.
    /// An optional seal additionally fences changes since preparation.
    fn check_reclaimable(&self, path: &Path, sealed: Option<&VcsSnapshot>) -> Result<(), VcsError> {
        let _ = (path, sealed);
        Err(VcsError::mint_unavailable(
            "workspace coverage cannot be established",
        ))
    }
    /// Trims only a proven ignored, untracked root build cache in a stopped copy.
    /// Callers own a reclamation intent and must stop all occupant jobs first.
    fn trim_disposable_target(&self, path: &Path) -> Result<bool, VcsError> {
        let _ = path;
        Ok(false)
    }
    /// Mints `agent_id`'s copy of `parent_cwd` under `launch`, over `base`
    /// when one is named and over the parent's own state otherwise.
    fn mint_from(
        &self,
        launch: &Path,
        agent_id: &str,
        copy_id: &str,
        parent_cwd: &Path,
        mode: WorkspaceMode,
        base: Option<&VcsSnapshot>,
    ) -> Result<MintedCopy, VcsError>;
    /// Whether the sealed JJ history has left its recorded mint base.
    fn history_left_base(
        &self,
        _path: &Path,
        _base: &str,
        _sealed: &str,
    ) -> Result<bool, VcsError> {
        Ok(false)
    }

    /// Seals what the copy changed as `candidate_id`, pinned until adoption or discard.
    fn seal(
        &self,
        child_path: &Path,
        namespace: &str,
        candidate_id: &str,
    ) -> Result<VcsSnapshot, VcsError>;
    /// Drops the candidate's pin.
    fn unpin(&self, child_path: &Path, namespace: &str, candidate_id: &str)
    -> Result<(), VcsError>;
    /// Lands `candidate` three-way onto `target_path`, refusing when the
    /// target moved past `expected_target` or the merge conflicts.
    fn adopt(
        &self,
        target_path: &Path,
        child_path: &Path,
        base_commit: &str,
        base_change_id: Option<&str>,
        candidate: &VcsSnapshot,
        expected_target: Option<&str>,
    ) -> Result<AdoptLanding, AdoptFailure>;
    /// Runs a template's `on_mint` hook in `cwd`; `Err` is `HookFailed`
    /// with the bounded output excerpt as its detail.
    fn run_mint_hook(
        &self,
        cwd: &Path,
        command: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<(), VcsError>;
    /// Removes the copy at `path` and whatever the mint registered for it.
    fn unmint(&self, launch: &Path, path: &Path) -> Result<(), VcsError>;
    /// Whether the copy has uncommitted changes.
    fn dirty(&self, path: &Path) -> bool;
    /// How far `child` has moved from `parent`, when both are readable.
    fn ahead_behind(&self, child: &Path, parent: &Path, base: Option<&str>) -> Option<(u64, u64)>;
    /// The copy's branch or revision, for presentation only.
    fn label(&self, path: &Path) -> Option<String>;
    /// jj only: whether `path`'s chain carries anything beyond its base.
    fn diverged_from_base(
        &self,
        path: &Path,
        base_change_id: Option<&str>,
        base_commit: &str,
    ) -> Result<bool, VcsError>;
    /// jj only: whether `copy_id`'s copy was minted over `candidate`.
    fn copy_is_over_candidate(&self, launch: &Path, copy_id: &str, candidate: &VcsSnapshot)
    -> bool;
}

/// The optional Git CLI / native JJ / shell hook implementation of [`VcsOps`]: the only one that ships.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemVcs;

impl VcsOps for SystemVcs {
    fn trim_disposable_target(&self, path: &Path) -> Result<bool, VcsError> {
        maintenance::trim_disposable_target(path)
    }

    fn check_reclaimable(&self, path: &Path, sealed: Option<&VcsSnapshot>) -> Result<(), VcsError> {
        maintenance::check_reclaimable(path, sealed)
    }
    fn mint_from(
        &self,
        launch: &Path,
        _agent_id: &str,
        copy_id: &str,
        parent_cwd: &Path,
        mode: WorkspaceMode,
        base: Option<&VcsSnapshot>,
    ) -> Result<MintedCopy, VcsError> {
        mint_worktree_from(launch, copy_id, parent_cwd, mode, base)
    }

    fn history_left_base(&self, path: &Path, base: &str, sealed: &str) -> Result<bool, VcsError> {
        match detect_vcs(path)? {
            SessionVcs::Jj => crate::native_jj::history_left_base(path, base, sealed),
            SessionVcs::Git => Ok(false),
        }
    }

    fn seal(
        &self,
        child_path: &Path,
        namespace: &str,
        candidate_id: &str,
    ) -> Result<VcsSnapshot, VcsError> {
        seal_candidate(child_path, namespace, candidate_id)
    }

    fn unpin(
        &self,
        child_path: &Path,
        namespace: &str,
        candidate_id: &str,
    ) -> Result<(), VcsError> {
        unpin_candidate(child_path, namespace, candidate_id)
    }

    fn adopt(
        &self,
        target_path: &Path,
        child_path: &Path,
        base_commit: &str,
        base_change_id: Option<&str>,
        candidate: &VcsSnapshot,
        expected_target: Option<&str>,
    ) -> Result<AdoptLanding, AdoptFailure> {
        adopt_candidate(
            target_path,
            child_path,
            base_commit,
            base_change_id,
            candidate,
            expected_target,
        )
    }

    fn run_mint_hook(
        &self,
        cwd: &Path,
        command: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<(), VcsError> {
        run_mint_hook(cwd, command, env).map_err(VcsError::hook_failed)
    }

    fn unmint(&self, launch: &Path, path: &Path) -> Result<(), VcsError> {
        unmint_copy(launch, path)
    }

    fn dirty(&self, path: &Path) -> bool {
        copy_is_dirty(path)
    }

    fn ahead_behind(&self, child: &Path, parent: &Path, base: Option<&str>) -> Option<(u64, u64)> {
        ahead_behind(child, parent, base)
    }

    fn label(&self, path: &Path) -> Option<String> {
        workspace_label(path)
    }

    fn diverged_from_base(
        &self,
        path: &Path,
        base_change_id: Option<&str>,
        base_commit: &str,
    ) -> Result<bool, VcsError> {
        jj_diverged_from_base(path, base_change_id, base_commit)
    }

    fn copy_is_over_candidate(
        &self,
        launch: &Path,
        copy_id: &str,
        candidate: &VcsSnapshot,
    ) -> bool {
        jj_copy_is_over_candidate(launch, copy_id, candidate)
    }
}
