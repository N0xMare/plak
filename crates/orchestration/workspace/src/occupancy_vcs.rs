//! Git worktree and jj workspace mint/seal/adopt.
//!
//! Session VCS prefers JJ when compiled in; Git-only builds use colocated Git.
//! Probe-fail is `mint_unavailable`.
//! The host drives mint/discard; models do not prelude `git worktree add` or
//! `jj workspace add`. JJ repository operations use the private native backend;
//! Git operations use the optional CLI backend.
//!
//! The one primitive is [`snapshot`]: it computes the base at
//! mint, the candidate at seal, and the target at adopt. Every operation in
//! this module is expected to run with the tree mutex **released**; nothing
//! here touches `ProductAgentTree`, and nothing here names a topology type:
//! failures are [`VcsError`], which the tree converts.
//!
//! [`VcsOps`] is the port the tree drives; [`SystemVcs`] is the only
//! production implementation. A test doubles it with `FakeVcs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

mod conflicts;
mod git;
mod jj;
pub mod maintenance;
mod mint;
mod names;
mod namespace;
mod ops;
mod pins;
mod refresh;

pub(crate) use git::git_query;
#[cfg(test)]
use git::{
    FALLBACK_EMAIL, GIT_QUERY_TIMEOUT, STDERR_EXCERPT_BYTES, git_command, git_text, output_within,
    parse_merge_tree_conflicts, stderr_excerpt, strip_git_environment,
};
use git::{
    ensure_plak_mint_excludes, git_adopt, git_ahead_behind, git_branch, git_run, git_snapshot,
    git_worktree_dirty, git_worktree_registered, reclaim_orphan_worktree, unmint_git_dest,
};
pub(super) use git::{is_secret_git_path, secret_fileset};
#[cfg(test)]
use jj::jj_detached_copy;
pub use jj::validate_jj_copy_base;
use jj::{jj_adopt, jj_ahead_behind, jj_mint_lock, jj_snapshot, jj_workspace_dirty};
pub(crate) use jj::{jj_copy_is_over_candidate, jj_diverged_from_base};
pub use mint::run_mint_hook;
#[cfg(test)]
use mint::run_mint_hook_bounded;
use mint::{mint_git_worktree, mint_jj_workspace};
pub use names::{CopyNames, branch_of_copy_dir, copy_names};
pub use ops::{AdoptRequest, MintRequest, RevertRange, SystemVcs, VcsOps};
pub use refresh::{RefreshApplied, RefreshPlan, RefreshPreparation, RefreshRequest};

use plakous_host_protocol::topology::WorkspaceMode;

#[cfg(feature = "git-cli")]
const GIT_CLI_ENABLED: bool = true;
#[cfg(not(feature = "git-cli"))]
const GIT_CLI_ENABLED: bool = false;

/// One `snapshot(path)` result. For git `content` is the written
/// tree id and `commit` is the `commit-tree` commit; for JJ `content` hashes
/// the native merged tree IDs and conflict labels with explicit framing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VcsSnapshot {
    /// Seal observation: the recorded mint base is no longer an ancestor.
    pub history_left_base: bool,
    /// The commit the snapshot is pinned to.
    pub commit: String,
    /// What the snapshot's content hashes to: a git tree id, or a digest of
    /// the native JJ tree identity.
    pub content: String,
    /// The jj change the snapshot sits on; `None` on git.
    pub change_id: Option<String>,
}

impl VcsSnapshot {
    /// The `expected_target` token: `<commit>:<content>`.
    pub(crate) fn token(&self) -> String {
        format!("{}:{}", self.commit, self.content)
    }

    /// Accepts the full token, the commit alone, or the content alone so a
    /// model can quote whichever id it was handed.
    pub(crate) fn matches_expectation(&self, expected: &str) -> bool {
        let expected = expected.trim();
        !expected.is_empty()
            && (expected == self.token() || expected == self.commit || expected == self.content)
    }
}

/// One freshly minted copy: where it is, what it is on, and
/// the base a later seal and adopt three-way against.
#[derive(Clone, Debug)]
pub struct MintedCopy {
    /// The copy's own directory.
    pub path: PathBuf,
    /// The branch a `fork` copy took; `None` for a detached `snapshot` copy.
    pub branch: Option<String>,
    /// The snapshot the copy was minted from.
    pub base: VcsSnapshot,
}

pub use plakous_host_protocol::topology::ConflictPath;

/// A pinned textual merge which has not changed the target checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictCandidate {
    /// Conflicted merge result, including text markers.
    pub snapshot: VcsSnapshot,
    /// Exact target snapshot used as the merge's base.
    pub base: VcsSnapshot,
}

/// Identity reserved by the tree for a conflict result.
#[derive(Clone, Debug)]
pub struct ConflictPin {
    /// Candidate id, including its reserved conflict ordinal.
    pub id: String,
    /// Stable wrapper label.
    pub label: String,
}

/// A resolution also accepts this source candidate and advances its copy.
pub struct ResolvedSource<'a> {
    /// Original work candidate's identity.
    pub candidate_id: &'a str,
    /// Retained copy's producer, absent after reclamation.
    pub agent_id: Option<&'a str>,
    /// Original work, never the conflict markers or resolver snapshot.
    pub commit: &'a str,
}

/// Why an adopt did not land. Never a success status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdoptFailure {
    /// The backend cannot apply this adoption mode; nothing was applied.
    NotApplicable(String),
    /// Repository publication or checkout needs explicit reconciliation.
    RecoveryRequired(String),
    /// Three-way merge conflicted; the child copy is retained untouched.
    Conflict {
        /// All conflicted paths and their kinds.
        paths: Vec<ConflictPath>,
        /// Present only for an ordinary, entirely textual Git conflict.
        candidate: Option<Box<ConflictCandidate>>,
    },
    /// The target moved under the adopt; nothing was applied. Carries the
    /// current target token.
    TargetChanged(String),
    /// The VCS could not be driven at all.
    Unavailable(String),
}

/// What a landed adopt changed in the caller's own copy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdoptLanding {
    /// The target's token before the landing.
    pub target_before: String,
    /// The target's token after it.
    pub target_after: String,
    /// How many files the landing touched.
    pub files_changed: u32,
    /// Every path the landing touched, in diff order. The receipt keeps at
    /// most [`ADOPT_RECEIPT_FILE_LIMIT`] of them.
    pub files: Vec<ChangedPath>,
    /// A replacement child base if a backend changes the child's tree while
    /// adopting. The Git and native JJ backends leave the tree unchanged and
    /// return `None`, so the adopted candidate is the next baseline.
    pub child_base: Option<VcsSnapshot>,
}

/// How many changed paths an adopt receipt names. `files_changed` stays the
/// full count.
pub const ADOPT_RECEIPT_FILE_LIMIT: usize = 64;

/// Added, modified, deleted, renamed, or a type change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    /// The path was added.
    Added,
    /// The path's contents changed.
    Modified,
    /// The path was removed.
    Deleted,
    /// The path was renamed. `from` names the old path.
    Renamed,
    /// The path changed type.
    TypeChanged,
}

/// One path a landing changed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedPath {
    /// What happened to the path.
    pub status: ChangeKind,
    /// The path after the change.
    pub path: String,
    /// The previous path of a rename.
    pub from: Option<String>,
}

impl ChangeKind {
    fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::TypeChanged => 'T',
        }
    }
}

impl ChangedPath {
    /// `"M src/lib.rs"` or `"R old.rs -> new.rs"`.
    #[must_use]
    pub fn receipt_line(&self) -> String {
        if self.status == ChangeKind::Renamed {
            let from = self.from.as_deref().unwrap_or("");
            return format!("R {from} -> {}", self.path);
        }
        format!("{} {}", self.status.letter(), self.path)
    }
}

impl AdoptLanding {
    /// Receipt file lines, cut at [`ADOPT_RECEIPT_FILE_LIMIT`].
    #[must_use]
    pub fn receipt_files(&self) -> (Vec<String>, bool) {
        let truncated = self.files.len() > ADOPT_RECEIPT_FILE_LIMIT;
        let files = self
            .files
            .iter()
            .take(ADOPT_RECEIPT_FILE_LIMIT)
            .map(ChangedPath::receipt_line)
            .collect();
        (files, truncated)
    }
}

/// Why a VCS operation did not run. Deliberately not a
/// topology error: this module knows nothing about the tree, and the tree
/// maps each kind onto its own wire code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VcsErrorKind {
    /// Work is preserved but repository or checkout state needs explicit recovery.
    RecoveryRequired,
    /// No git or jj identity, or the tool could not be driven at all.
    MintUnavailable,
    /// The copy could not be minted.
    MintFailed,
    /// The candidate could not be pinned or its divergence could not be read.
    SealFailed,
    /// The `on_mint` hook failed or timed out.
    HookFailed,
}

/// Coverage evidence governing whether disposable cache trimming is safe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReclamationRefusal {
    /// Static content cannot be covered by normal sealing.
    StaticCoverage,
    /// The working copy changed after its acknowledged seal.
    ChangedAfterSeal,
}

/// A refusal from the VCS, typed so the caller can name it on its own wire
/// without this crate knowing that wire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcsError {
    /// What kind of refusal it is.
    pub kind: VcsErrorKind,
    /// The bounded operator-facing detail.
    pub detail: String,
    /// Typed coverage refusal, absent for operational or unclassified failures.
    pub reclamation_refusal: Option<ReclamationRefusal>,
}

impl VcsError {
    /// A refusal of `kind` with `detail`.
    #[must_use]
    pub fn new(kind: VcsErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            reclamation_refusal: None,
        }
    }

    /// Creates a coverage refusal without classifying operational errors as static.
    #[must_use]
    pub fn reclamation_refused(class: ReclamationRefusal, detail: impl Into<String>) -> Self {
        Self {
            reclamation_refusal: Some(class),
            ..Self::mint_unavailable(detail)
        }
    }

    /// No `git` or `jj` identity to mint from.
    #[must_use]
    pub fn mint_unavailable(detail: impl Into<String>) -> Self {
        Self::new(VcsErrorKind::MintUnavailable, detail)
    }

    /// The VCS was there but refused the mint.
    #[must_use]
    pub fn mint_failed(detail: impl Into<String>) -> Self {
        Self::new(VcsErrorKind::MintFailed, detail)
    }

    /// The seal could not be written or pinned.
    #[must_use]
    pub fn seal_failed(detail: impl Into<String>) -> Self {
        Self::new(VcsErrorKind::SealFailed, detail)
    }

    /// A template's `on_mint` hook exited non-zero or timed out.
    #[must_use]
    pub fn hook_failed(detail: impl Into<String>) -> Self {
        Self::new(VcsErrorKind::HookFailed, detail)
    }
}

impl std::fmt::Display for VcsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionVcs {
    Git,
    Jj,
}

/// Whether this path is exactly a minted JJ copy root, not a nested directory.
#[must_use]
pub fn is_jj_copy_root(path: &Path) -> bool {
    copy_layout_kind(path) == Some(SessionVcs::Jj)
}

/// Only the leaf's parent and grandparent identify a minted copy.
pub(crate) fn copy_layout_kind(path: &Path) -> Option<SessionVcs> {
    let parent = path.parent()?;
    if path.file_name().is_none() || parent.parent()?.file_name()? != ".plak" {
        return None;
    }
    match parent.file_name()?.to_str()? {
        "worktrees" => Some(SessionVcs::Git),
        "workspaces" => Some(SessionVcs::Jj),
        _ => None,
    }
}

impl SessionVcs {
    /// The copy's directory decides the backend. `.plak/worktrees/` is git and
    /// `.plak/workspaces/` is jj even after the launch checkout is colocated.
    /// The layout is the leaf's grandparent and parent, not an earlier
    /// `.plak` directory the path happens to contain. Anything else is
    /// [`detect_vcs`].
    pub(crate) fn of_copy(path: &Path) -> Result<Self, VcsError> {
        if let Some(kind) = copy_layout_kind(path) {
            return Ok(kind);
        }
        detect_vcs(path)
    }
}

pub(crate) fn detect_vcs(launch: &Path) -> Result<SessionVcs, VcsError> {
    if launch.join(".jj").exists() && super::native_jj::ENABLED {
        Ok(SessionVcs::Jj)
    } else if launch.join(".git").exists() {
        if !GIT_CLI_ENABLED {
            return Err(VcsError::mint_unavailable(
                "Git CLI backend is not included in this build",
            ));
        }
        Ok(SessionVcs::Git)
    } else if launch.join(".jj").exists() {
        Err(VcsError::mint_unavailable(
            "this build has no jj support; use a build with the jj feature",
        ))
    } else {
        Err(VcsError::mint_unavailable(
            "launch checkout has no git or jj identity",
        ))
    }
}

/// `snapshot(path) -> { commit, content, change_id }`.
///
/// git: a temp index seeded from `git rev-parse --git-path index` (**not**
/// `.git/index`, which is the wrong file inside a worktree), `git add -A`, the
/// secret unstage, `write-tree`, `commit-tree`. The working copy, the real
/// index, and `HEAD` are all left exactly as they were.
pub fn snapshot(path: &Path, label: &str) -> Result<VcsSnapshot, VcsError> {
    match detect_vcs(path).or_else(|_| detect_parent_vcs(path))? {
        SessionVcs::Git => git_snapshot(path, label),
        SessionVcs::Jj => jj_snapshot(path),
    }
}

/// A fork or snapshot copy of the parent's state. The base is
/// the parent's `snapshot`, so the copy starts byte-identical to what the
/// parent had — no dirty-file copy, and no launch-vs-parent HEAD mismatch.
/// Mints a copy over an explicit base, or over the parent's live state.
///
/// A base is what a `spawn_agent` `inputs` entry means: the child
/// starts from a sealed candidate, not from whatever its parent's checkout
/// happens to hold now. It is the candidate's own snapshot, so the copy's merge
/// base is the candidate and a later adopt is still three-way.
pub(crate) fn mint_worktree_from(request: &MintRequest<'_>) -> Result<MintedCopy, VcsError> {
    match detect_vcs(request.launch)? {
        SessionVcs::Git => mint_git_worktree(request),
        SessionVcs::Jj => mint_jj_workspace(request),
    }
}

/// Seals `child_path` into a candidate: snapshot the copy and,
/// on git, pin the commit under `refs/plak/<namespace>/candidates/<candidate_id>` so
/// discarding the worktree and its `plak/<tree8>/<agent_id>` branch cannot orphan it. Native
/// JJ pins the exact commit in its Git backend under the same ref namespace.
pub(crate) fn seal_candidate(
    child_path: &Path,
    namespace: &str,
    candidate_id: &str,
    base_commit: &str,
) -> Result<VcsSnapshot, VcsError> {
    let vcs = SessionVcs::of_copy(child_path)?;
    let namespace_guard = if vcs == SessionVcs::Git {
        let output = git_query(child_path, &["rev-parse", "--git-common-dir"])
            .map_err(|error| VcsError::seal_failed(error.to_string()))?;
        if !output.status.success() {
            return Err(VcsError::seal_failed(
                "cannot locate namespace ownership directory",
            ));
        }
        let common = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        Some(namespace::reserve(&child_path.join(common), namespace)?)
    } else {
        None
    };
    let sealed = match vcs {
        SessionVcs::Git => git_snapshot(child_path, candidate_id),
        SessionVcs::Jj => jj_snapshot(child_path),
    }?;
    drop(namespace_guard);
    let refs = [
        (
            candidate_ref(namespace, candidate_id),
            Some(sealed.commit.as_str()),
        ),
        (
            pins::reference(namespace, "candidate-bases", candidate_id),
            Some(base_commit),
        ),
    ];
    match vcs {
        SessionVcs::Git => pins::update(
            child_path,
            namespace,
            &refs,
            candidate_id.starts_with("failed-"),
        )?,
        SessionVcs::Jj => crate::native_jj::pin_references(
            child_path,
            namespace,
            &refs,
            candidate_id.starts_with("failed-"),
        )?,
    }
    Ok(sealed)
}

/// The ref a sealed candidate is pinned by, so the commit survives until the
/// candidate record is collected or its unadopted work is explicitly discarded.
#[must_use]
pub fn candidate_ref(namespace: &str, candidate_id: &str) -> String {
    format!(
        "refs/plak/{}/candidates/{candidate_id}",
        namespace.get(..16).unwrap_or(namespace)
    )
}

/// All refs owned by a retained candidate record, including landing provenance.
#[must_use]
pub fn candidate_refs(namespace: &str, candidate_id: &str) -> [String; 4] {
    [
        candidate_ref(namespace, candidate_id),
        pins::reference(namespace, "candidate-bases", candidate_id),
        pins::reference(namespace, "adopted", &format!("{candidate_id}-before")),
        pins::reference(namespace, "adopted", &format!("{candidate_id}-after")),
    ]
}

/// Ref retaining the base of one live copy independently of its input record.
#[must_use]
pub fn copy_base_ref(namespace: &str, agent: &str) -> String {
    pins::reference(namespace, "bases", agent)
}

#[cfg(feature = "jj-lib")]
pub(crate) fn reserve_candidate_namespace(
    common: &Path,
    digest: &str,
) -> Result<std::fs::File, VcsError> {
    namespace::reserve(common, digest)
}

pub(crate) fn reserve_copy_namespace(
    common: &Path,
    digest: &str,
) -> Result<std::fs::File, VcsError> {
    namespace::reserve_prefix(common, digest)
}

pub(crate) fn copy_namespace_owner(common: &Path, dir: &str) -> Result<(String, String), VcsError> {
    let (tree8, agent) = names::copy_parts(dir).ok_or_else(|| {
        VcsError::mint_unavailable("unrecognized copy identity; artifacts retained")
    })?;
    Ok((namespace::owner(common, tree8)?, agent.to_owned()))
}

/// Drops the pin `seal_candidate` placed when the snapshot turned out not to
/// diverge from its base and no candidate is recorded. Without
/// this the tree reports `candidate: null` while the ref still exists (live
/// 2026-09-08 D-B). The native JJ backend removes the same private Git ref.
pub(crate) fn unpin_candidate(
    child_path: &Path,
    namespace: &str,
    candidate_id: &str,
) -> Result<(), VcsError> {
    let refs = [
        (candidate_ref(namespace, candidate_id), None),
        (
            pins::reference(namespace, "candidate-bases", candidate_id),
            None,
        ),
        (
            pins::reference(namespace, "adopted", &format!("{candidate_id}-before")),
            None,
        ),
        (
            pins::reference(namespace, "adopted", &format!("{candidate_id}-after")),
            None,
        ),
    ];
    match SessionVcs::of_copy(child_path)? {
        SessionVcs::Git => pins::update(child_path, namespace, &refs, false),
        SessionVcs::Jj => crate::native_jj::pin_references(child_path, namespace, &refs, false),
    }
}

/// Three-way landing of a sealed candidate into `target_path`.
/// Git updates target files; JJ records and checks out a working-copy commit.
/// Neither backend changes child files.
pub(crate) fn adopt_candidate(request: &AdoptRequest<'_>) -> Result<AdoptLanding, AdoptFailure> {
    let AdoptRequest {
        target_path,
        child_path,
        base_commit,
        base_change_id,
        candidate,
        expected_target,
        ..
    } = *request;
    match detect_vcs(target_path).or_else(|_| detect_parent_vcs(target_path)) {
        Ok(SessionVcs::Git) => git_adopt(request),
        Ok(SessionVcs::Jj) if request.resolution_refusal.is_some() => Err(
            AdoptFailure::NotApplicable(request.resolution_refusal.unwrap_or_default().to_owned()),
        ),
        Ok(SessionVcs::Jj) if request.revert.is_some() => Err(AdoptFailure::NotApplicable(
            "revert is not yet available for jj".to_owned(),
        )),
        Ok(SessionVcs::Jj) if request.resolved_source.is_some() => {
            Err(AdoptFailure::NotApplicable(
                "conflict candidates are not yet available for jj".to_owned(),
            ))
        }
        Ok(SessionVcs::Jj) if matches!(SessionVcs::of_copy(child_path), Ok(SessionVcs::Git)) => {
            Err(AdoptFailure::Unavailable(
                "git candidate into a jj target: not yet supported".to_owned(),
            ))
        }
        Ok(SessionVcs::Jj) => {
            let landing = jj_adopt(
                target_path,
                child_path,
                base_commit,
                base_change_id,
                candidate,
                expected_target,
            )
            .map_err(|failure| {
                if request.accept_conflicts && matches!(failure, AdoptFailure::Conflict { .. }) {
                    AdoptFailure::NotApplicable(
                        "conflict candidates are not yet available for jj".to_owned(),
                    )
                } else {
                    failure
                }
            })?;
            let refs = pins::adoption(
                request,
                landing
                    .target_before
                    .split(':')
                    .next()
                    .unwrap_or(&landing.target_before),
                landing
                    .target_after
                    .split(':')
                    .next()
                    .unwrap_or(&landing.target_after),
            );
            crate::native_jj::pin_references(
                target_path,
                request.namespace,
                &refs
                    .iter()
                    .map(|(name, value)| (name.clone(), Some(value.as_str())))
                    .collect::<Vec<_>>(),
                false,
            )
            .map_err(|error| AdoptFailure::RecoveryRequired(error.detail))?;
            Ok(landing)
        }
        Err(error) => Err(AdoptFailure::Unavailable(error.detail)),
    }
}

/// Waits for `child` (spawned as its own process group) until `timeout`.
/// `Ok(None)` means the ceiling passed: the whole group was killed and reaped,
/// so nothing it started outlives the call.
fn wait_bounded(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let group = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw);
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if started.elapsed() >= timeout {
            if let Some(group) = group {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Unmints the copy. `refs/plak/*/candidates/*` is deliberately untouched: the
/// worktree and its `plak/<tree8>/<agent_id>` branch go, the sealed commit stays reachable
/// through its pin.
pub(crate) fn unmint_copy(launch: &Path, path: &Path, namespace: &str) -> Result<(), VcsError> {
    let agent_id = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| VcsError::mint_unavailable("workspace path has no identity"))?;
    let branch = branch_of_copy_dir(agent_id)
        .ok_or_else(|| VcsError::mint_unavailable("unrecognized copy directory; retained"))?;
    let (tree8, logical_agent) = names::copy_parts(agent_id)
        .ok_or_else(|| VcsError::mint_unavailable("invalid copy identity"))?;
    if namespace.get(..8) != Some(tree8) {
        return Err(VcsError::mint_unavailable(
            "copy belongs to another tree; retained",
        ));
    }
    match SessionVcs::of_copy(path)? {
        SessionVcs::Jj => {
            crate::native_jj::forget(launch, agent_id, namespace)?;
            std::fs::remove_dir_all(path)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(|_| {
                    VcsError::new(
                        VcsErrorKind::RecoveryRequired,
                        "workspace was forgotten but directory removal failed",
                    )
                })?;
        }
        SessionVcs::Git => {
            let common = pins::common(launch)?;
            if copy_namespace_owner(&common, agent_id)?.0 != namespace {
                return Err(VcsError::mint_unavailable(
                    "copy namespace owner differs; retained",
                ));
            }
            let _namespace_guard = reserve_copy_namespace(&common, namespace)?;
            let path_text = path.display().to_string();
            // A copy whose directory and registration are both gone was
            // already removed, by an earlier attempt or by hand; only its
            // branch may remain, and `git worktree remove` refuses the path.
            let directory_gone = std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            if !directory_gone || git_worktree_registered(launch, path)? {
                let output = git_run(
                    launch,
                    &["worktree", "remove", "--force", path_text.as_str()],
                )
                .map_err(|_| {
                    VcsError::new(
                        VcsErrorKind::RecoveryRequired,
                        "Git worktree removal could not run",
                    )
                })?;
                if !output.status.success() {
                    return Err(VcsError::new(
                        VcsErrorKind::RecoveryRequired,
                        "Git worktree removal failed; directory or registration remains",
                    ));
                }
            }
            let reference = format!("refs/heads/{branch}");
            let present = git_run(
                launch,
                &["show-ref", "--verify", "--quiet", reference.as_str()],
            )
            .map_err(|_| {
                VcsError::new(
                    VcsErrorKind::RecoveryRequired,
                    "worktree removed but branch state could not be checked",
                )
            })?;
            if present.status.success() {
                let removed =
                    git_run(launch, &["branch", "-D", branch.as_str()]).map_err(|_| {
                        VcsError::new(
                            VcsErrorKind::RecoveryRequired,
                            "worktree removed but branch cleanup could not run",
                        )
                    })?;
                if !removed.status.success() {
                    return Err(VcsError::new(
                        VcsErrorKind::RecoveryRequired,
                        "worktree removed but branch cleanup failed",
                    ));
                }
            } else if present.status.code() != Some(1) {
                return Err(VcsError::new(
                    VcsErrorKind::RecoveryRequired,
                    "worktree removed but branch presence is uncertain",
                ));
            }
            pins::update_reserved(
                launch,
                &[(pins::reference(namespace, "bases", logical_agent), None)],
                false,
            )?;
        }
    }
    Ok(())
}

pub(crate) fn copy_is_dirty(path: &Path) -> bool {
    match SessionVcs::of_copy(path) {
        Ok(SessionVcs::Jj) => jj_workspace_dirty(path),
        _ => git_worktree_dirty(path),
    }
}

/// `(ahead, behind)` of `child` relative to `parent`. Omit from list JSON when `None`.
/// The child's layout chooses the backend, not the parent's current VCS.
pub(crate) fn ahead_behind(child: &Path, parent: &Path, base: Option<&str>) -> Option<(u64, u64)> {
    match SessionVcs::of_copy(child) {
        Ok(SessionVcs::Jj) => jj_ahead_behind(child, parent, base),
        Ok(SessionVcs::Git) => git_ahead_behind(child, parent, base),
        Err(_) => None,
    }
}

/// Parent observations shared only within one occupancy-probe run, by path and base.
pub type ParentBehindCache = BTreeMap<(PathBuf, String), Option<u64>>;

pub(crate) fn ahead_behind_cached(
    child: &Path,
    parent: &Path,
    base: Option<&str>,
    cache: &mut ParentBehindCache,
) -> Option<(u64, u64)> {
    if copy_layout_kind(child) == Some(SessionVcs::Git) {
        git::git_ahead_behind_cached(child, parent, base, cache)
    } else {
        ahead_behind(child, parent, base)
    }
}

fn detect_parent_vcs(path: &Path) -> Result<SessionVcs, VcsError> {
    for ancestor in path.ancestors() {
        if ancestor.join(".jj").exists() || ancestor.join(".git").exists() {
            return detect_vcs(ancestor);
        }
    }
    Err(VcsError::mint_unavailable("path has no git or jj identity"))
}

/// Display label for `list_agents` `branch` (git branch or jj workspace name).
pub(crate) fn workspace_label(path: &Path) -> Option<String> {
    match SessionVcs::of_copy(path) {
        Ok(SessionVcs::Jj) => {
            let name = path.file_name()?.to_str()?;
            if path
                .ancestors()
                .any(|ancestor| ancestor.file_name() == Some(std::ffi::OsStr::new("workspaces")))
            {
                Some(format!("plak-{name}"))
            } else {
                Some("default".to_owned())
            }
        }
        Ok(SessionVcs::Git) => git_branch(path),
        Err(_) => None,
    }
}

/// A fresh owner-only (0700) directory for one step's scratch files; it and
/// everything in it are removed when the handle drops.
fn private_scratch() -> std::io::Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt as _;
    tempfile::Builder::new()
        .prefix("plak-vcs-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
}

/// Creates a new owner-only (0600) file; never follows or reuses an existing one.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

pub(super) fn append_exclude_lines(path: &Path) -> Result<(), VcsError> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut extra = String::new();
    for line in [".plak/worktrees/", ".plak/workspaces/", ".plak/bin/"] {
        if !existing.lines().any(|row| row.trim() == line) {
            extra.push_str(line);
            extra.push('\n');
        }
    }
    if extra.is_empty() {
        return Ok(());
    }
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&extra);
    std::fs::write(path, body).map_err(|_| mint_error("exclude could not be written"))
}

fn mint_error(what: &str) -> VcsError {
    VcsError::mint_unavailable(what)
}

/// Lowercase hex SHA-256 of `value`; shared with the tree's request digests.
pub fn digest_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// A recording [`VcsOps`] double for tree tests: no subprocess, no repo. Every
/// call is logged. Minted copies are plain directories under
/// `<launch>/.plak/fake/<agent_id>`; seals diverge by default, and a seal, an
/// adopt, or a hook can be pointed at an explicit answer for one call.
/// Compiled for this crate's tests and, under the `test-support` feature,
/// for the crates the topology moves into at the cut.
#[cfg(any(test, feature = "test-support"))]
pub mod fake;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
