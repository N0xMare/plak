//! The [`VcsOps`] port and the production [`SystemVcs`] implementation.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use super::{
    AdoptFailure, AdoptLanding, BTreeMap, ConflictPin, MintedCopy, Path, ResolvedSource,
    SessionVcs, VcsError, VcsErrorKind, VcsSnapshot, WorkspaceMode, adopt_candidate, ahead_behind,
    copy_is_dirty, detect_vcs, jj_copy_is_over_candidate, jj_diverged_from_base, maintenance,
    mint_worktree_from, run_mint_hook, seal_candidate, unmint_copy, unpin_candidate,
    workspace_label,
};

/// One [`VcsOps`] answer, still being computed.
pub type VcsFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn ready<'a, T: Send + 'a>(value: T) -> VcsFuture<'a, T> {
    Box::pin(std::future::ready(value))
}

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
    /// Reverse the pinned adoption rather than landing the candidate.
    pub revert: Option<&'a RevertRange>,
    /// Explicit permission to land marker-bearing work.
    pub accept_conflicts: bool,
    /// Reserved id for a new textual conflict, absent on resolution retries.
    pub conflict_pin: Option<&'a ConflictPin>,
    /// Paths whose markers a resolver must remove unless explicitly accepted.
    pub resolver_paths: &'a [String],
    /// Original candidate accepted by a conflict resolution.
    pub resolved_source: Option<ResolvedSource<'a>>,
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

/// Immutable adoption provenance and conditional copy-base restorations.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RevertRange {
    /// Pinned target snapshot before adoption.
    pub before: String,
    /// Pinned exact result of adoption, used as the reverse merge base.
    pub after: String,
    /// Producing agent and original base, only for copies still at the adopted base.
    pub restore_bases: Vec<(String, String)>,
}

/// The VCS port the tree drives. Every method is a repository operation or
/// a filesystem question about a copy or a checkout, answered where the
/// workspace lives, and every answer is awaited: the tree never calls one
/// under its mutex or inside a writer, and never holds the mutex across a
/// seal, an adopt, a hook, or an unmint.
///
/// A question whose answer gates a destructive step or an adoption is
/// fallible, and an `Err` is never an "absent" answer: a port that cannot
/// reach the workspace answers `Err`, and the caller fails closed.
pub trait VcsOps: Send + Sync {
    /// Current placement admission for `path`, without I/O. `None` denotes
    /// an in-process port. A closed barrier defers unstarted seals and new
    /// turns, never retries a dispatched stage.
    fn placement_ready(&self, path: &Path) -> Option<bool> {
        let _ = path;
        None
    }

    /// The runner this port's effects run on when it is not this process:
    /// its effects are then recorded as tree stages before they run, and
    /// each runs under its stage's key (see [`super::stage`]). `None`, the
    /// default, is a port that works in this process.
    fn stage_runner(&self) -> Option<&str> {
        None
    }

    /// The port of `runner`, another runner this tree's port reaches for a
    /// copy that crosses runners; `None` when it reaches no such runner.
    fn transfer_port(&self, runner: &str) -> Option<std::sync::Arc<dyn VcsOps>> {
        let _ = runner;
        None
    }

    /// The tree's mirror repository on this port's runner, where copies
    /// that crossed runners are minted from.
    fn transfer_root<'a>(&'a self, tree_id: &'a str) -> VcsFuture<'a, Result<PathBuf, VcsError>> {
        let _ = tree_id;
        ready(Err(VcsError::mint_unavailable(
            "this runner keeps no copies from other runners",
        )))
    }

    /// Pins the parent's state as a base snapshot to carry to a copy on
    /// another runner.
    fn snapshot_base<'a>(
        &'a self,
        parent_cwd: &'a Path,
        namespace: &'a str,
        agent: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        let _ = (parent_cwd, namespace, agent);
        ready(Err(transfer_unavailable()))
    }

    /// Bundles `reference` of `repo` without what `exclude` names.
    fn bundle_create<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
        exclude: &'a [String],
    ) -> VcsFuture<'a, Result<super::BundleFacts, VcsError>> {
        let _ = (repo, reference, exclude);
        ready(Err(transfer_unavailable()))
    }

    /// Up to `len` bytes of bundle `id` from `offset`.
    fn bundle_read<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        len: u64,
    ) -> VcsFuture<'a, Result<Vec<u8>, VcsError>> {
        let _ = (repo, id, offset, len);
        ready(Err(transfer_unavailable()))
    }

    /// Removes bundle `id`.
    fn bundle_drop<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let _ = (repo, id);
        ready(Err(transfer_unavailable()))
    }

    /// Writes one chunk of upload `id` into `repo` at `offset`.
    fn bundle_write<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        bytes: &'a [u8],
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let _ = (repo, id, offset, bytes);
        ready(Err(transfer_unavailable()))
    }

    /// Applies upload `id` to `repo` once it hashes to `digest`: the
    /// `fetch` refs from the bundle, then the `pins` under `namespace`.
    fn bundle_apply<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        digest: &'a str,
        namespace: &'a str,
        fetch: &'a [(String, String)],
        pins: &'a [(String, String)],
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let _ = (repo, id, digest, namespace, fetch, pins);
        ready(Err(transfer_unavailable()))
    }

    /// The commit `reference` names in `repo`, if any.
    fn ref_commit<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
    ) -> VcsFuture<'a, Result<Option<String>, VcsError>> {
        let _ = (repo, reference);
        ready(Err(transfer_unavailable()))
    }

    /// Removes the transfer files under `repo` older than `max_age_secs`
    /// that no carry finished; how many it removed. A port that carries no
    /// copies has none.
    fn sweep_transfers<'a>(
        &'a self,
        repo: &'a Path,
        max_age_secs: u64,
    ) -> VcsFuture<'a, Result<u32, VcsError>> {
        let _ = (repo, max_age_secs);
        ready(Ok(0))
    }

    /// Computes a clean live-fork catch-up using object writes only.
    fn prepare_refresh<'a>(
        &'a self,
        _request: &'a super::RefreshRequest,
    ) -> VcsFuture<'a, Result<super::RefreshPreparation, VcsError>> {
        ready(Ok(super::RefreshPreparation::Skipped(
            "refresh is unavailable for this backend".into(),
        )))
    }

    /// Applies or reconciles the exact previously published refresh plan.
    fn apply_refresh<'a>(
        &'a self,
        _plan: &'a super::RefreshPlan,
        _recover: bool,
    ) -> VcsFuture<'a, Result<super::RefreshApplied, VcsError>> {
        ready(Err(VcsError::mint_unavailable(
            "refresh recovery is unavailable for this backend",
        )))
    }
    /// Proves normal sealing covers every non-disposable byte before reclaim.
    /// An optional seal additionally fences changes since preparation.
    fn check_reclaimable<'a>(
        &'a self,
        path: &'a Path,
        sealed: Option<&'a VcsSnapshot>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let _ = (path, sealed);
        ready(Err(VcsError::mint_unavailable(
            "workspace coverage cannot be established",
        )))
    }
    /// Trims only a proven ignored, untracked root build cache in a stopped copy.
    /// Callers own a reclamation intent and must stop all occupant jobs first.
    fn trim_disposable_target<'a>(
        &'a self,
        path: &'a Path,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        let _ = path;
        ready(Ok(false))
    }
    /// Whether `path` names an existing filesystem object, following links.
    /// `Err` when the workspace cannot be asked.
    fn path_exists<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Result<bool, VcsError>>;
    /// The names of the existing entries of `<launch>/.plak/workspaces/`, the
    /// JJ copy root, including a copy a crashed mint left without a row.
    /// `Err` when the workspace cannot be asked.
    fn jj_copy_dirs<'a>(
        &'a self,
        launch: &'a Path,
    ) -> VcsFuture<'a, Result<BTreeSet<String>, VcsError>>;
    /// Mints `agent_id`'s copy of `parent_cwd` under `launch`, over `base`
    /// when one is named and over the parent's own state otherwise.
    fn mint_from<'a>(
        &'a self,
        request: &'a MintRequest<'_>,
    ) -> VcsFuture<'a, Result<MintedCopy, VcsError>>;
    /// Whether the sealed JJ history has left its recorded mint base.
    fn history_left_base<'a>(
        &'a self,
        _path: &'a Path,
        _base: &'a str,
        _sealed: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        ready(Ok(false))
    }

    /// Seals what the copy changed as `candidate_id`, pinned while its record is retained.
    fn seal<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>>;
    /// Drops the candidate's pin.
    fn unpin<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>>;
    /// Mirrors the committed workspace base during interrupted-adopt recovery.
    fn restore_copy_base<'a>(
        &'a self,
        path: &'a Path,
        namespace: &'a str,
        agent: &'a str,
        base: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>>;
    /// Lands `candidate` three-way onto `target_path`, refusing when the
    /// target moved past `expected_target` or the merge conflicts.
    fn adopt<'a>(
        &'a self,
        request: &'a AdoptRequest<'_>,
    ) -> VcsFuture<'a, Result<AdoptLanding, AdoptFailure>>;
    /// Runs a template's `on_mint` hook in `cwd`; `Err` is `HookFailed`
    /// with the bounded output excerpt as its detail.
    fn run_mint_hook<'a>(
        &'a self,
        cwd: &'a Path,
        command: &'a str,
        env: &'a BTreeMap<String, String>,
    ) -> VcsFuture<'a, Result<(), VcsError>>;
    /// Removes the copy at `path` and whatever the mint registered for it.
    fn unmint<'a>(
        &'a self,
        launch: &'a Path,
        path: &'a Path,
        namespace: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>>;
    /// Whether the copy has uncommitted changes, for presentation only.
    fn dirty<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, bool>;
    /// How far `child` has moved from `parent`, when both are readable; for
    /// presentation only.
    fn ahead_behind<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
    ) -> VcsFuture<'a, Option<(u64, u64)>>;
    /// Reuses parent observations for one render, never across probe runs.
    fn ahead_behind_cached<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
        _cache: &'a mut super::ParentBehindCache,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        self.ahead_behind(child, parent, base)
    }
    /// The copy's branch or revision, for presentation only.
    fn label<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Option<String>>;
    /// jj only: whether `path`'s chain carries anything beyond its base.
    fn diverged_from_base<'a>(
        &'a self,
        path: &'a Path,
        base_change_id: Option<&'a str>,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>>;
    /// jj only: whether `copy_id`'s copy was minted over `candidate`.
    fn copy_is_over_candidate<'a>(
        &'a self,
        launch: &'a Path,
        copy_id: &'a str,
        candidate: &'a VcsSnapshot,
    ) -> VcsFuture<'a, Result<bool, VcsError>>;
}

fn transfer_unavailable() -> VcsError {
    VcsError::mint_unavailable("this port does not carry copies between runners")
}

/// The optional Git CLI / native JJ / shell hook implementation of [`VcsOps`]: the only one that ships.
///
/// Its repository work blocks, so each answer runs on Tokio's blocking pool
/// when a runtime is current (and on the polling thread otherwise). Once
/// polled, the work runs to its end even if the future is dropped.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemVcs;

/// Runs `work` on a blocking worker of the current runtime, or here when
/// there is none. A worker lost with its runtime answers `lost`; a panic in
/// `work` resumes in the poller.
fn blocking<'a, T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    lost: impl FnOnce() -> T + Send + 'a,
) -> VcsFuture<'a, T> {
    Box::pin(async move {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return work();
        };
        match runtime.spawn_blocking(work).await {
            Ok(value) => value,
            Err(error) => match error.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                Err(_) => lost(),
            },
        }
    })
}

/// A presence question the filesystem could not answer.
fn presence_unknown(path: &Path, error: &std::io::Error) -> VcsError {
    VcsError::mint_unavailable(format!(
        "{} could not be inspected ({})",
        path.display(),
        error.kind()
    ))
}

fn lost_worker() -> VcsError {
    VcsError::mint_unavailable("the VCS worker was cancelled with its runtime")
}

/// [`MintRequest`] owned, so its work can move to a blocking worker or
/// across a runner link.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OwnedMint {
    launch: PathBuf,
    agent_id: String,
    namespace: String,
    parent_cwd: PathBuf,
    mode: WorkspaceMode,
    base: Option<VcsSnapshot>,
}

impl OwnedMint {
    /// `request`, owned.
    #[must_use]
    pub fn of(request: &MintRequest<'_>) -> Self {
        Self {
            launch: request.launch.to_owned(),
            agent_id: request.agent_id.to_owned(),
            namespace: request.namespace.to_owned(),
            parent_cwd: request.parent_cwd.to_owned(),
            mode: request.mode,
            base: request.base.cloned(),
        }
    }

    /// The request again, borrowed.
    #[must_use]
    pub fn request(&self) -> MintRequest<'_> {
        MintRequest {
            launch: &self.launch,
            agent_id: &self.agent_id,
            namespace: &self.namespace,
            parent_cwd: &self.parent_cwd,
            mode: self.mode,
            base: self.base.as_ref(),
        }
    }
}

/// [`AdoptRequest`] owned, so its work can move to a blocking worker or
/// across a runner link.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OwnedAdopt {
    resolution_refusal: Option<String>,
    revert: Option<RevertRange>,
    accept_conflicts: bool,
    conflict_pin: Option<ConflictPin>,
    resolver_paths: Vec<String>,
    resolved_source: Option<(String, Option<String>, String)>,
    target_path: PathBuf,
    child_path: PathBuf,
    base_commit: String,
    base_change_id: Option<String>,
    candidate: VcsSnapshot,
    expected_target: Option<String>,
    namespace: String,
    candidate_id: String,
    source_agent_id: Option<String>,
}

impl OwnedAdopt {
    /// `request`, owned.
    #[must_use]
    pub fn of(request: &AdoptRequest<'_>) -> Self {
        Self {
            resolution_refusal: request.resolution_refusal.map(str::to_owned),
            revert: request.revert.cloned(),
            accept_conflicts: request.accept_conflicts,
            conflict_pin: request.conflict_pin.cloned(),
            resolver_paths: request.resolver_paths.to_vec(),
            resolved_source: request.resolved_source.as_ref().map(|source| {
                (
                    source.candidate_id.to_owned(),
                    source.agent_id.map(str::to_owned),
                    source.commit.to_owned(),
                )
            }),
            target_path: request.target_path.to_owned(),
            child_path: request.child_path.to_owned(),
            base_commit: request.base_commit.to_owned(),
            base_change_id: request.base_change_id.map(str::to_owned),
            candidate: request.candidate.clone(),
            expected_target: request.expected_target.map(str::to_owned),
            namespace: request.namespace.to_owned(),
            candidate_id: request.candidate_id.to_owned(),
            source_agent_id: request.source_agent_id.map(str::to_owned),
        }
    }

    /// The request again, borrowed.
    #[must_use]
    pub fn request(&self) -> AdoptRequest<'_> {
        AdoptRequest {
            resolution_refusal: self.resolution_refusal.as_deref(),
            revert: self.revert.as_ref(),
            accept_conflicts: self.accept_conflicts,
            conflict_pin: self.conflict_pin.as_ref(),
            resolver_paths: &self.resolver_paths,
            resolved_source: self.resolved_source.as_ref().map(
                |(candidate_id, agent_id, commit)| ResolvedSource {
                    candidate_id,
                    agent_id: agent_id.as_deref(),
                    commit,
                },
            ),
            target_path: &self.target_path,
            child_path: &self.child_path,
            base_commit: &self.base_commit,
            base_change_id: self.base_change_id.as_deref(),
            candidate: &self.candidate,
            expected_target: self.expected_target.as_deref(),
            namespace: &self.namespace,
            candidate_id: &self.candidate_id,
            source_agent_id: self.source_agent_id.as_deref(),
        }
    }
}

/// The blocking implementations [`VcsOps`] runs on a blocking worker, one
/// per port method and under its name.
#[allow(
    clippy::unused_self,
    reason = "method form keeps the port's names for the in-crate repository tests"
)]
impl SystemVcs {
    pub(crate) fn prepare_refresh(
        self,
        request: &super::RefreshRequest,
    ) -> Result<super::RefreshPreparation, VcsError> {
        super::refresh::prepare(request)
    }

    pub(crate) fn apply_refresh(
        self,
        plan: &super::RefreshPlan,
        recover: bool,
    ) -> Result<super::RefreshApplied, VcsError> {
        super::refresh::apply(plan, recover)
    }

    pub(crate) fn restore_copy_base(
        self,
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

    pub(crate) fn trim_disposable_target(self, path: &Path) -> Result<bool, VcsError> {
        maintenance::trim_disposable_target(path)
    }

    pub(crate) fn check_reclaimable(
        self,
        path: &Path,
        sealed: Option<&VcsSnapshot>,
    ) -> Result<(), VcsError> {
        maintenance::check_reclaimable(path, sealed)
    }

    /// Whether `path` exists. A path that cannot be inspected (permission,
    /// I/O) is an error, never "absent".
    pub(crate) fn path_exists(self, path: &Path) -> Result<bool, VcsError> {
        path.try_exists()
            .map_err(|error| presence_unknown(path, &error))
    }

    /// The directory names under the JJ copy root. A root that does not exist
    /// lists nothing; one that cannot be read, or an entry that cannot be
    /// inspected, is an error rather than an empty or shorter listing.
    pub(crate) fn jj_copy_dirs(self, launch: &Path) -> Result<BTreeSet<String>, VcsError> {
        let root = launch.join(".plak/workspaces");
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeSet::new());
            }
            Err(error) => return Err(presence_unknown(&root, &error)),
        };
        let mut dirs = BTreeSet::new();
        for entry in entries {
            let entry = entry.map_err(|error| presence_unknown(&root, &error))?;
            let path = entry.path();
            if !path
                .try_exists()
                .map_err(|error| presence_unknown(&path, &error))?
            {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                dirs.insert(name);
            }
        }
        Ok(dirs)
    }

    pub(crate) fn mint_from(self, request: &MintRequest<'_>) -> Result<MintedCopy, VcsError> {
        mint_worktree_from(request)
    }

    pub(crate) fn history_left_base(
        self,
        path: &Path,
        base: &str,
        sealed: &str,
    ) -> Result<bool, VcsError> {
        match detect_vcs(path)? {
            SessionVcs::Jj => crate::native_jj::history_left_base(path, base, sealed),
            SessionVcs::Git => Ok(false),
        }
    }

    pub(crate) fn seal(
        self,
        child_path: &Path,
        namespace: &str,
        candidate_id: &str,
        base_commit: &str,
    ) -> Result<VcsSnapshot, VcsError> {
        seal_candidate(child_path, namespace, candidate_id, base_commit)
    }

    pub(crate) fn unpin(
        self,
        child_path: &Path,
        namespace: &str,
        candidate_id: &str,
    ) -> Result<(), VcsError> {
        unpin_candidate(child_path, namespace, candidate_id)
    }

    pub(crate) fn adopt(self, request: &AdoptRequest<'_>) -> Result<AdoptLanding, AdoptFailure> {
        adopt_candidate(request)
    }

    pub(crate) fn run_mint_hook(
        self,
        cwd: &Path,
        command: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<(), VcsError> {
        run_mint_hook(cwd, command, env).map_err(VcsError::hook_failed)
    }

    pub(crate) fn unmint(
        self,
        launch: &Path,
        path: &Path,
        namespace: &str,
    ) -> Result<(), VcsError> {
        unmint_copy(launch, path, namespace)
    }

    pub(crate) fn dirty(self, path: &Path) -> bool {
        copy_is_dirty(path)
    }

    pub(crate) fn ahead_behind(
        self,
        child: &Path,
        parent: &Path,
        base: Option<&str>,
    ) -> Option<(u64, u64)> {
        ahead_behind(child, parent, base)
    }

    pub(crate) fn ahead_behind_cached(
        self,
        child: &Path,
        parent: &Path,
        base: Option<&str>,
        cache: &mut super::ParentBehindCache,
    ) -> Option<(u64, u64)> {
        super::ahead_behind_cached(child, parent, base, cache)
    }

    pub(crate) fn label(self, path: &Path) -> Option<String> {
        workspace_label(path)
    }

    pub(crate) fn diverged_from_base(
        self,
        path: &Path,
        base_change_id: Option<&str>,
        base_commit: &str,
    ) -> Result<bool, VcsError> {
        jj_diverged_from_base(path, base_change_id, base_commit)
    }

    pub(crate) fn copy_is_over_candidate(
        self,
        launch: &Path,
        copy_id: &str,
        candidate: &VcsSnapshot,
    ) -> bool {
        jj_copy_is_over_candidate(launch, copy_id, candidate)
    }
}

impl VcsOps for SystemVcs {
    fn snapshot_base<'a>(
        &'a self,
        parent_cwd: &'a Path,
        namespace: &'a str,
        agent: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        let (cwd, namespace, agent) = (
            parent_cwd.to_owned(),
            namespace.to_owned(),
            agent.to_owned(),
        );
        blocking(
            move || super::transfer::snapshot_base(&cwd, &namespace, &agent),
            || Err(lost_worker()),
        )
    }

    fn bundle_create<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
        exclude: &'a [String],
    ) -> VcsFuture<'a, Result<super::BundleFacts, VcsError>> {
        let (repo, reference, exclude) = (repo.to_owned(), reference.to_owned(), exclude.to_vec());
        blocking(
            move || super::transfer::bundle_create(&repo, &reference, &exclude),
            || Err(lost_worker()),
        )
    }

    fn bundle_read<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        len: u64,
    ) -> VcsFuture<'a, Result<Vec<u8>, VcsError>> {
        let (repo, id) = (repo.to_owned(), id.to_owned());
        blocking(
            move || super::transfer::bundle_read(&repo, &id, offset, len),
            || Err(lost_worker()),
        )
    }

    fn bundle_drop<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (repo, id) = (repo.to_owned(), id.to_owned());
        blocking(
            move || super::transfer::bundle_drop(&repo, &id),
            || Err(lost_worker()),
        )
    }

    fn bundle_write<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        bytes: &'a [u8],
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (repo, id, bytes) = (repo.to_owned(), id.to_owned(), bytes.to_vec());
        blocking(
            move || super::transfer::bundle_write(&repo, &id, offset, &bytes),
            || Err(lost_worker()),
        )
    }

    fn bundle_apply<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        digest: &'a str,
        namespace: &'a str,
        fetch: &'a [(String, String)],
        pins: &'a [(String, String)],
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (repo, id, digest, namespace, fetch, pins) = (
            repo.to_owned(),
            id.to_owned(),
            digest.to_owned(),
            namespace.to_owned(),
            fetch.to_vec(),
            pins.to_vec(),
        );
        blocking(
            move || super::transfer::bundle_apply(&repo, &id, &digest, &namespace, &fetch, &pins),
            || Err(lost_worker()),
        )
    }

    fn ref_commit<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
    ) -> VcsFuture<'a, Result<Option<String>, VcsError>> {
        let (repo, reference) = (repo.to_owned(), reference.to_owned());
        blocking(
            move || super::transfer::ref_commit(&repo, &reference),
            || Err(lost_worker()),
        )
    }

    fn sweep_transfers<'a>(
        &'a self,
        repo: &'a Path,
        max_age_secs: u64,
    ) -> VcsFuture<'a, Result<u32, VcsError>> {
        let repo = repo.to_owned();
        blocking(
            move || super::transfer::sweep(&repo, std::time::Duration::from_secs(max_age_secs)),
            || Err(lost_worker()),
        )
    }

    fn prepare_refresh<'a>(
        &'a self,
        request: &'a super::RefreshRequest,
    ) -> VcsFuture<'a, Result<super::RefreshPreparation, VcsError>> {
        let request = request.clone();
        blocking(
            move || Self.prepare_refresh(&request),
            || Err(lost_worker()),
        )
    }

    fn apply_refresh<'a>(
        &'a self,
        plan: &'a super::RefreshPlan,
        recover: bool,
    ) -> VcsFuture<'a, Result<super::RefreshApplied, VcsError>> {
        let plan = plan.clone();
        blocking(
            move || Self.apply_refresh(&plan, recover),
            || Err(lost_worker()),
        )
    }

    fn restore_copy_base<'a>(
        &'a self,
        path: &'a Path,
        namespace: &'a str,
        agent: &'a str,
        base: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (path, namespace, agent, base) = (
            path.to_owned(),
            namespace.to_owned(),
            agent.to_owned(),
            base.to_owned(),
        );
        blocking(
            move || Self.restore_copy_base(&path, &namespace, &agent, &base),
            || Err(lost_worker()),
        )
    }

    fn trim_disposable_target<'a>(
        &'a self,
        path: &'a Path,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        let path = path.to_owned();
        blocking(
            move || Self.trim_disposable_target(&path),
            || Err(lost_worker()),
        )
    }

    fn check_reclaimable<'a>(
        &'a self,
        path: &'a Path,
        sealed: Option<&'a VcsSnapshot>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (path, sealed) = (path.to_owned(), sealed.cloned());
        blocking(
            move || Self.check_reclaimable(&path, sealed.as_ref()),
            || Err(lost_worker()),
        )
    }

    fn path_exists<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Result<bool, VcsError>> {
        let path = path.to_owned();
        blocking(move || Self.path_exists(&path), || Err(lost_worker()))
    }

    fn jj_copy_dirs<'a>(
        &'a self,
        launch: &'a Path,
    ) -> VcsFuture<'a, Result<BTreeSet<String>, VcsError>> {
        let launch = launch.to_owned();
        blocking(move || Self.jj_copy_dirs(&launch), || Err(lost_worker()))
    }

    fn mint_from<'a>(
        &'a self,
        request: &'a MintRequest<'_>,
    ) -> VcsFuture<'a, Result<MintedCopy, VcsError>> {
        let request = OwnedMint::of(request);
        blocking(
            move || Self.mint_from(&request.request()),
            || Err(lost_worker()),
        )
    }

    fn history_left_base<'a>(
        &'a self,
        path: &'a Path,
        base: &'a str,
        sealed: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        let (path, base, sealed) = (path.to_owned(), base.to_owned(), sealed.to_owned());
        blocking(
            move || Self.history_left_base(&path, &base, &sealed),
            || Err(lost_worker()),
        )
    }

    fn seal<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        let (path, namespace, candidate, base) = (
            child_path.to_owned(),
            namespace.to_owned(),
            candidate_id.to_owned(),
            base_commit.to_owned(),
        );
        blocking(
            move || Self.seal(&path, &namespace, &candidate, &base),
            || Err(lost_worker()),
        )
    }

    fn unpin<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (path, namespace, candidate) = (
            child_path.to_owned(),
            namespace.to_owned(),
            candidate_id.to_owned(),
        );
        blocking(
            move || Self.unpin(&path, &namespace, &candidate),
            || Err(lost_worker()),
        )
    }

    fn adopt<'a>(
        &'a self,
        request: &'a AdoptRequest<'_>,
    ) -> VcsFuture<'a, Result<AdoptLanding, AdoptFailure>> {
        let request = OwnedAdopt::of(request);
        blocking(
            move || Self.adopt(&request.request()),
            || Err(AdoptFailure::Unavailable(lost_worker().detail)),
        )
    }

    fn run_mint_hook<'a>(
        &'a self,
        cwd: &'a Path,
        command: &'a str,
        env: &'a BTreeMap<String, String>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (cwd, command, env) = (cwd.to_owned(), command.to_owned(), env.clone());
        blocking(
            move || Self.run_mint_hook(&cwd, &command, &env),
            || {
                Err(VcsError::new(
                    VcsErrorKind::HookFailed,
                    lost_worker().detail,
                ))
            },
        )
    }

    fn unmint<'a>(
        &'a self,
        launch: &'a Path,
        path: &'a Path,
        namespace: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (launch, path, namespace) = (launch.to_owned(), path.to_owned(), namespace.to_owned());
        blocking(
            move || Self.unmint(&launch, &path, &namespace),
            || Err(lost_worker()),
        )
    }

    fn dirty<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, bool> {
        let path = path.to_owned();
        blocking(move || Self.dirty(&path), || false)
    }

    fn ahead_behind<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        let (child, parent, base) = (child.to_owned(), parent.to_owned(), base.map(str::to_owned));
        blocking(
            move || Self.ahead_behind(&child, &parent, base.as_deref()),
            || None,
        )
    }

    fn ahead_behind_cached<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
        cache: &'a mut super::ParentBehindCache,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        let (child, parent, base) = (child.to_owned(), parent.to_owned(), base.map(str::to_owned));
        let mut owned = std::mem::take(cache);
        Box::pin(async move {
            // The cache travels with the work and comes back with its answer;
            // a lost worker loses only this render's reuse.
            let (answer, owned) = blocking(
                move || {
                    let answer =
                        Self.ahead_behind_cached(&child, &parent, base.as_deref(), &mut owned);
                    (answer, owned)
                },
                || (None, super::ParentBehindCache::new()),
            )
            .await;
            *cache = owned;
            answer
        })
    }

    fn label<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Option<String>> {
        let path = path.to_owned();
        blocking(move || Self.label(&path), || None)
    }

    fn diverged_from_base<'a>(
        &'a self,
        path: &'a Path,
        base_change_id: Option<&'a str>,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        let (path, change, commit) = (
            path.to_owned(),
            base_change_id.map(str::to_owned),
            base_commit.to_owned(),
        );
        blocking(
            move || Self.diverged_from_base(&path, change.as_deref(), &commit),
            || Err(lost_worker()),
        )
    }

    fn copy_is_over_candidate<'a>(
        &'a self,
        launch: &'a Path,
        copy_id: &'a str,
        candidate: &'a VcsSnapshot,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        let (launch, copy_id, candidate) =
            (launch.to_owned(), copy_id.to_owned(), candidate.clone());
        blocking(
            move || Ok(Self.copy_is_over_candidate(&launch, &copy_id, &candidate)),
            || Err(lost_worker()),
        )
    }
}

/// The port of a session whose workspace lives on another runner: version
/// control there is not carried yet, so every question that gates an effect
/// is refused, never answered as absent, and presentation reads see nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableVcs;

fn unavailable() -> VcsError {
    VcsError::mint_unavailable("version control on another runner is not carried yet")
}

impl VcsOps for UnavailableVcs {
    fn path_exists<'a>(&'a self, _path: &'a Path) -> VcsFuture<'a, Result<bool, VcsError>> {
        ready(Err(unavailable()))
    }

    fn jj_copy_dirs<'a>(
        &'a self,
        _launch: &'a Path,
    ) -> VcsFuture<'a, Result<BTreeSet<String>, VcsError>> {
        ready(Err(unavailable()))
    }

    fn mint_from<'a>(
        &'a self,
        _request: &'a MintRequest<'_>,
    ) -> VcsFuture<'a, Result<MintedCopy, VcsError>> {
        ready(Err(unavailable()))
    }

    fn seal<'a>(
        &'a self,
        _child_path: &'a Path,
        _namespace: &'a str,
        _candidate_id: &'a str,
        _base_commit: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        ready(Err(unavailable()))
    }

    fn unpin<'a>(
        &'a self,
        _child_path: &'a Path,
        _namespace: &'a str,
        _candidate_id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        ready(Err(unavailable()))
    }

    fn restore_copy_base<'a>(
        &'a self,
        _path: &'a Path,
        _namespace: &'a str,
        _agent: &'a str,
        _base: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        ready(Err(unavailable()))
    }

    fn adopt<'a>(
        &'a self,
        _request: &'a AdoptRequest<'_>,
    ) -> VcsFuture<'a, Result<AdoptLanding, AdoptFailure>> {
        ready(Err(AdoptFailure::Unavailable(unavailable().detail)))
    }

    fn run_mint_hook<'a>(
        &'a self,
        _cwd: &'a Path,
        _command: &'a str,
        _env: &'a BTreeMap<String, String>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        ready(Err(unavailable()))
    }

    fn unmint<'a>(
        &'a self,
        _launch: &'a Path,
        _path: &'a Path,
        _namespace: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        ready(Err(unavailable()))
    }

    fn dirty<'a>(&'a self, _path: &'a Path) -> VcsFuture<'a, bool> {
        ready(false)
    }

    fn ahead_behind<'a>(
        &'a self,
        _child: &'a Path,
        _parent: &'a Path,
        _base: Option<&'a str>,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        ready(None)
    }

    fn label<'a>(&'a self, _path: &'a Path) -> VcsFuture<'a, Option<String>> {
        ready(None)
    }

    fn diverged_from_base<'a>(
        &'a self,
        _path: &'a Path,
        _base_change_id: Option<&'a str>,
        _base_commit: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        ready(Err(unavailable()))
    }

    fn copy_is_over_candidate<'a>(
        &'a self,
        _launch: &'a Path,
        _copy_id: &'a str,
        _candidate: &'a VcsSnapshot,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        ready(Err(unavailable()))
    }
}
