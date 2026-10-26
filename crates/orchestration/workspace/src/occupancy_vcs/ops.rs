//! The [`VcsOps`] port and the production [`SystemVcs`] implementation.

use super::{
    AdoptFailure, AdoptLanding, BTreeMap, MintedCopy, Path, SessionVcs, VcsError, VcsSnapshot,
    WorkspaceMode, adopt_candidate, ahead_behind, copy_is_dirty, detect_vcs,
    jj_copy_is_over_candidate, jj_diverged_from_base, maintenance, mint_worktree_from,
    run_mint_hook, seal_candidate, unmint_copy, unpin_candidate, workspace_label,
};

/// Identity and immutable inputs for one copy mint.
pub struct MintRequest<'a> {
    /// Checkout owning all copies.
    pub launch: &'a Path,
    /// Logical owner of the new copy.
    pub agent_id: &'a str,
    /// Full tree digest owning its pins.
    pub namespace: &'a str,
    /// Caller state to copy when no candidate is supplied.
    pub parent_cwd: &'a Path,
    /// Fork or detached snapshot.
    pub mode: WorkspaceMode,
    /// An explicit candidate input, otherwise the caller's live state.
    pub base: Option<&'a VcsSnapshot>,
}

/// One three-way landing and the identity of its retained provenance.
pub struct AdoptRequest<'a> {
    /// A spent resolution may be inspected for markers, but must never apply.
    pub resolution_refusal: Option<&'a str>,
    /// Explicit permission to land marker-bearing work.
    pub accept_conflicts: bool,
    /// Reserved id for a new textual conflict, absent on resolution retries.
    pub conflict_pin: Option<&'a super::ConflictPin>,
    /// Paths whose markers a resolver must remove unless explicitly accepted.
    pub resolver_paths: &'a [String],
    /// Original candidate accepted by a conflict resolution.
    pub resolved_source: Option<super::ResolvedSource<'a>>,
    /// Checkout receiving the patch.
    pub target_path: &'a Path,
    /// Copy that produced the candidate.
    pub child_path: &'a Path,
    /// Immutable merge base.
    pub base_commit: &'a str,
    /// JJ lineage at that base.
    pub base_change_id: Option<&'a str>,
    /// Immutable candidate snapshot.
    pub candidate: &'a VcsSnapshot,
    /// Optional target concurrency fence.
    pub expected_target: Option<&'a str>,
    /// Full tree digest owning the provenance refs.
    pub namespace: &'a str,
    /// Candidate whose landing is recorded.
    pub candidate_id: &'a str,
    /// Base pin to advance for a retained producing copy; absent after reclamation.
    pub source_agent_id: Option<&'a str>,
}

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
    fn mint_from(&self, request: &MintRequest<'_>) -> Result<MintedCopy, VcsError>;
    /// Whether the sealed JJ history has left its recorded mint base.
    fn history_left_base(
        &self,
        _path: &Path,
        _base: &str,
        _sealed: &str,
    ) -> Result<bool, VcsError> {
        Ok(false)
    }

    /// Seals what the copy changed as `candidate_id`, pinned while its record is retained.
    fn seal(
        &self,
        child_path: &Path,
        namespace: &str,
        candidate_id: &str,
        base_commit: &str,
    ) -> Result<VcsSnapshot, VcsError>;
    /// Drops the candidate's pin.
    fn unpin(&self, child_path: &Path, namespace: &str, candidate_id: &str)
    -> Result<(), VcsError>;
    /// Mirrors the committed workspace base during interrupted-adopt recovery.
    fn restore_copy_base(
        &self,
        path: &Path,
        namespace: &str,
        agent: &str,
        base: &str,
    ) -> Result<(), VcsError>;
    /// Lands `candidate` three-way onto `target_path`, refusing when the
    /// target moved past `expected_target` or the merge conflicts.
    fn adopt(&self, request: &AdoptRequest<'_>) -> Result<AdoptLanding, AdoptFailure>;
    /// Runs a template's `on_mint` hook in `cwd`; `Err` is `HookFailed`
    /// with the bounded output excerpt as its detail.
    fn run_mint_hook(
        &self,
        cwd: &Path,
        command: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<(), VcsError>;
    /// Removes the copy at `path` and whatever the mint registered for it.
    fn unmint(&self, launch: &Path, path: &Path, namespace: &str) -> Result<(), VcsError>;
    /// Whether the copy has uncommitted changes.
    fn dirty(&self, path: &Path) -> bool;
    /// How far `child` has moved from `parent`, when both are readable.
    fn ahead_behind(&self, child: &Path, parent: &Path, base: Option<&str>) -> Option<(u64, u64)>;
    /// Reuses parent observations for one render, never across probe runs.
    fn ahead_behind_cached(
        &self,
        child: &Path,
        parent: &Path,
        base: Option<&str>,
        _cache: &mut super::ParentBehindCache,
    ) -> Option<(u64, u64)> {
        self.ahead_behind(child, parent, base)
    }
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
    fn restore_copy_base(
        &self,
        path: &Path,
        namespace: &str,
        agent: &str,
        base: &str,
    ) -> Result<(), VcsError> {
        let refs = [(super::copy_base_ref(namespace, agent), Some(base))];
        match SessionVcs::of_copy(path)? {
            SessionVcs::Git => super::pins::update(path, namespace, &refs, false),
            SessionVcs::Jj => crate::native_jj::pin_references(path, namespace, &refs, false),
        }
    }

    fn trim_disposable_target(&self, path: &Path) -> Result<bool, VcsError> {
        maintenance::trim_disposable_target(path)
    }

    fn check_reclaimable(&self, path: &Path, sealed: Option<&VcsSnapshot>) -> Result<(), VcsError> {
        maintenance::check_reclaimable(path, sealed)
    }
    fn mint_from(&self, request: &MintRequest<'_>) -> Result<MintedCopy, VcsError> {
        mint_worktree_from(request)
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
        base_commit: &str,
    ) -> Result<VcsSnapshot, VcsError> {
        seal_candidate(child_path, namespace, candidate_id, base_commit)
    }

    fn unpin(
        &self,
        child_path: &Path,
        namespace: &str,
        candidate_id: &str,
    ) -> Result<(), VcsError> {
        unpin_candidate(child_path, namespace, candidate_id)
    }

    fn adopt(&self, request: &AdoptRequest<'_>) -> Result<AdoptLanding, AdoptFailure> {
        adopt_candidate(request)
    }

    fn run_mint_hook(
        &self,
        cwd: &Path,
        command: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<(), VcsError> {
        run_mint_hook(cwd, command, env).map_err(VcsError::hook_failed)
    }

    fn unmint(&self, launch: &Path, path: &Path, namespace: &str) -> Result<(), VcsError> {
        unmint_copy(launch, path, namespace)
    }

    fn dirty(&self, path: &Path) -> bool {
        copy_is_dirty(path)
    }

    fn ahead_behind(&self, child: &Path, parent: &Path, base: Option<&str>) -> Option<(u64, u64)> {
        ahead_behind(child, parent, base)
    }

    fn ahead_behind_cached(
        &self,
        child: &Path,
        parent: &Path,
        base: Option<&str>,
        cache: &mut super::ParentBehindCache,
    ) -> Option<(u64, u64)> {
        super::ahead_behind_cached(child, parent, base, cache)
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
