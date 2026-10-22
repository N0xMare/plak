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
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

pub mod maintenance;
mod namespace;

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

/// Why an adopt did not land. Never a success status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdoptFailure {
    /// Repository publication or checkout needs explicit reconciliation.
    RecoveryRequired(String),
    /// Three-way merge conflicted; the child copy is retained untouched.
    Conflict(Vec<String>),
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
    /// A replacement child base if a backend changes the child's tree while
    /// adopting. The Git and native JJ backends leave the tree unchanged and
    /// return `None`, so the adopted candidate is the next baseline.
    pub child_base: Option<VcsSnapshot>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionVcs {
    Git,
    Jj,
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
pub(crate) fn mint_worktree_from(
    launch: &Path,
    agent_id: &str,
    parent_cwd: &Path,
    mode: WorkspaceMode,
    base: Option<&VcsSnapshot>,
) -> Result<MintedCopy, VcsError> {
    match detect_vcs(launch)? {
        SessionVcs::Git => mint_git_worktree(launch, agent_id, parent_cwd, mode, base),
        SessionVcs::Jj => mint_jj_workspace(launch, agent_id, parent_cwd, base),
    }
}

/// Seals `child_path` into a candidate: snapshot the copy and,
/// on git, pin the commit under `refs/plak/<namespace>/candidates/<candidate_id>` so
/// discarding the worktree and its `plak/<id>` branch cannot orphan it. Native
/// JJ pins the exact commit in its Git backend under the same ref namespace.
pub(crate) fn seal_candidate(
    child_path: &Path,
    namespace: &str,
    candidate_id: &str,
) -> Result<VcsSnapshot, VcsError> {
    let vcs = detect_vcs(child_path).or_else(|_| detect_parent_vcs(child_path))?;
    let _namespace_guard = if vcs == SessionVcs::Git {
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
    let sealed = snapshot(child_path, candidate_id)?;
    if vcs == SessionVcs::Git {
        let reference = candidate_ref(namespace, candidate_id);
        let mut args = vec!["update-ref", reference.as_str(), sealed.commit.as_str()];
        if candidate_id.starts_with("failed-") {
            // An empty old value means "must not exist" in every object
            // format; a 40-zero id is not a valid name in SHA-256 repositories.
            args.push("");
        }
        let output = git_run(child_path, &args)
            .map_err(|_| VcsError::seal_failed("git update-ref could not pin the candidate"))?;
        if !output.status.success() {
            return Err(VcsError::seal_failed(format!(
                "git update-ref refused the candidate pin {reference}; any existing ref was preserved"
            )));
        }
    }
    if vcs == SessionVcs::Jj {
        crate::native_jj::pin(child_path, namespace, candidate_id, Some(&sealed.commit))?;
    }
    Ok(sealed)
}

/// The ref a sealed candidate is pinned by, so the commit survives until the
/// candidate is adopted or discarded.
#[must_use]
pub fn candidate_ref(namespace: &str, candidate_id: &str) -> String {
    format!(
        "refs/plak/{}/candidates/{candidate_id}",
        namespace.get(..16).unwrap_or(namespace)
    )
}

#[cfg(feature = "jj-lib")]
pub(crate) fn reserve_candidate_namespace(
    common: &Path,
    digest: &str,
) -> Result<std::fs::File, VcsError> {
    namespace::reserve(common, digest)
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
    match detect_vcs(child_path).or_else(|_| detect_parent_vcs(child_path))? {
        SessionVcs::Jj => crate::native_jj::pin(child_path, namespace, candidate_id, None),
        SessionVcs::Git => {
            let reference = candidate_ref(namespace, candidate_id);
            let output =
                git_run(child_path, &["update-ref", "-d", reference.as_str()]).map_err(|_| {
                    VcsError::new(
                        VcsErrorKind::RecoveryRequired,
                        "candidate unpin could not run",
                    )
                })?;
            if !output.status.success() {
                return Err(VcsError::new(
                    VcsErrorKind::RecoveryRequired,
                    "candidate unpin failed; reference may remain",
                ));
            }
            Ok(())
        }
    }
}

/// Three-way landing of a sealed candidate into `target_path`.
/// Git updates target files; JJ records and checks out a working-copy commit.
/// Neither backend changes child files.
pub(crate) fn adopt_candidate(
    target_path: &Path,
    child_path: &Path,
    base_commit: &str,
    base_change_id: Option<&str>,
    candidate: &VcsSnapshot,
    expected_target: Option<&str>,
) -> Result<AdoptLanding, AdoptFailure> {
    match detect_vcs(target_path).or_else(|_| detect_parent_vcs(target_path)) {
        Ok(SessionVcs::Git) => {
            git_adopt(target_path, base_commit, &candidate.commit, expected_target)
        }
        Ok(SessionVcs::Jj) => jj_adopt(
            target_path,
            child_path,
            base_commit,
            base_change_id,
            candidate,
            expected_target,
        ),
        Err(error) => Err(AdoptFailure::Unavailable(error.detail)),
    }
}

/// Ceiling on one `on_mint` hook run.
pub(crate) const MINT_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// How much hook output a failure message quotes.
const MINT_HOOK_OUTPUT_BYTES: usize = 2048;
/// Runs a template's `on_mint` command once in the new copy:
/// a shell command with the supplied hook environment over a *cleared* environment,
/// cwd the copy, bounded by `MINT_HOOK_TIMEOUT`. Output is captured into a
/// scratch file so a chatty hook cannot block on a full pipe. Expected to run
/// with the tree mutex released. `Err` is the typed reason the spawn fails.
///
/// The only host variables that cross into the hook are `PATH` — so it can
/// find `cargo`/`npm` — and `HOME`, for their caches; they are read one name
/// at a time so the workspace gate can see exactly what crosses. Everything
/// else is dropped: `secrets.rs` reads `*_API_KEY` from the environment when
/// set, and a template-authored shell command has no business seeing the
/// user's keys. A hook that needs more declares it in its own `[env]`.
/// The tree adds `PLAK_LAUNCH_CHECKOUT`, `PLAK_PARENT_PATH`, and `PLAK_COPY_ID`
/// to the supplied map, overriding same-named template entries; the runner
/// forwards them without reading any additional host variables.
///
/// The command is handed to `sh -c` — the module's third approved process
/// constructor in `tools/checks/check_workspace.py`, alongside `git` and `jj` — so
/// it runs in `cwd` with no repository discovery in the way.
pub fn run_mint_hook(
    cwd: &Path,
    command: &str,
    env: &BTreeMap<String, String>,
) -> Result<(), String> {
    run_mint_hook_bounded(cwd, command, env, MINT_HOOK_TIMEOUT)
}

/// [`run_mint_hook`] with the ceiling spelled out, so a test need not wait
/// [`MINT_HOOK_TIMEOUT`] to watch the timeout work.
fn run_mint_hook_bounded(
    cwd: &Path,
    command: &str,
    env: &BTreeMap<String, String>,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let scratch_dir =
        private_scratch().map_err(|error| format!("hook scratch was unavailable: {error}"))?;
    let scratch = scratch_dir.path().join("output");
    let log = create_private(&scratch)
        .map_err(|error| format!("hook output file could not be created: {error}"))?;
    let log_err = log
        .try_clone()
        .map_err(|error| format!("hook output file could not be shared: {error}"))?;
    let mut command_line = Command::new("sh");
    command_line
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err);
    if let Ok(value) = std::env::var("PATH") {
        command_line.env("PATH", value);
    }
    if let Ok(value) = std::env::var("HOME") {
        command_line.env("HOME", value);
    }
    command_line.envs(env);
    // The hook is a shell, and a shell's children outlive `kill(sh)`: a hook
    // that timed out left `cargo` running on the copy's target lock. Its own
    // process group makes the timeout able to reach everything it started.
    std::os::unix::process::CommandExt::process_group(&mut command_line, 0);
    let mut child = command_line
        .spawn()
        .map_err(|error| format!("hook could not start: {error}"))?;
    let status = match wait_bounded(&mut child, timeout) {
        Ok(Some(status)) => Ok(status),
        Ok(None) => Err(format!(
            "hook exceeded {}s and was killed",
            timeout.as_secs()
        )),
        Err(error) => Err(format!("hook could not be waited on: {error}")),
    };
    let output = std::fs::read(&scratch).unwrap_or_default();
    drop(scratch_dir);
    let status = status?;
    if status.success() {
        return Ok(());
    }
    let tail_start = output.len().saturating_sub(MINT_HOOK_OUTPUT_BYTES);
    let tail = String::from_utf8_lossy(&output[tail_start..])
        .trim()
        .to_owned();
    Err(format!(
        "hook exited {}: {}",
        status
            .code()
            .map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
        if tail.is_empty() {
            "(no output)".to_owned()
        } else {
            tail
        }
    ))
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

/// Ceiling on a `git` read: status, rev-parse, counts, name lists.
const GIT_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Ceiling on a `git` step that mints, seals, adopts, or unmints a copy.
const GIT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Runs `command` to completion or to `timeout`, whichever comes first. A
/// `git` that stalls (a lock, a credential prompt, a hung filesystem) fails
/// with `TimedOut` instead of holding the seal, adopt, or mint forever.
///
/// Output goes to scratch files, as the mint hook's does: nothing the command
/// leaves running can hold a pipe this process must drain.
fn output_within(mut command: Command, timeout: std::time::Duration) -> std::io::Result<Output> {
    let scratch = private_scratch()?;
    let stdout_path = scratch.path().join("stdout");
    let stderr_path = scratch.path().join("stderr");
    let captured = (|| {
        command
            .stdin(std::process::Stdio::null())
            .stdout(create_private(&stdout_path)?)
            .stderr(create_private(&stderr_path)?);
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn()?;
        match wait_bounded(&mut child, timeout)? {
            Some(status) => Ok(Output {
                status,
                stdout: std::fs::read(&stdout_path)?,
                stderr: std::fs::read(&stderr_path)?,
            }),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("git exceeded {}s and was killed", timeout.as_secs()),
            )),
        }
    })();
    drop(scratch);
    captured
}

/// Unmints the copy. `refs/plak/*/candidates/*` is deliberately untouched: the
/// worktree and its `plak/<id>` branch go, the sealed commit stays reachable
/// through its pin.
pub(crate) fn unmint_copy(launch: &Path, path: &Path) -> Result<(), VcsError> {
    let agent_id = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| VcsError::mint_unavailable("workspace path has no identity"))?;
    match detect_vcs(launch)? {
        SessionVcs::Jj => {
            crate::native_jj::forget(launch, agent_id)?;
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
            let branch = format!("plak/{agent_id}");
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
        }
    }
    Ok(())
}

/// Whether `git worktree list` still names `path`. Git records the real
/// path, so a missing directory is also compared through the real path of its
/// deepest existing ancestor.
fn git_worktree_registered(launch: &Path, path: &Path) -> Result<bool, VcsError> {
    let listed = git_run(launch, &["worktree", "list", "--porcelain", "-z"])
        .ok()
        .filter(|output| output.status.success())
        .ok_or_else(|| {
            VcsError::new(
                VcsErrorKind::RecoveryRequired,
                "Git worktree registrations could not be listed",
            )
        })?;
    let mut spellings = vec![path.to_path_buf()];
    if let Some(real) = path.ancestors().find_map(|ancestor| {
        let real = ancestor.canonicalize().ok()?;
        Some(real.join(path.strip_prefix(ancestor).ok()?))
    }) {
        spellings.push(real);
    }
    Ok(listed.stdout.split(|byte| *byte == 0).any(|field| {
        spellings
            .iter()
            .any(|spelling| field == format!("worktree {}", spelling.display()).as_bytes())
    }))
}

pub(crate) fn copy_is_dirty(path: &Path) -> bool {
    match detect_vcs(path).or_else(|_| detect_parent_vcs(path)) {
        Ok(SessionVcs::Jj) => jj_workspace_dirty(path),
        _ => git_worktree_dirty(path),
    }
}

/// `(ahead, behind)` of `child` relative to `parent`. Omit from list JSON when `None`.
pub(crate) fn ahead_behind(child: &Path, parent: &Path, base: Option<&str>) -> Option<(u64, u64)> {
    match detect_vcs(parent).or_else(|_| detect_vcs(child)) {
        Ok(SessionVcs::Jj) => jj_ahead_behind(child, parent, base),
        Ok(SessionVcs::Git) => git_ahead_behind(child, parent),
        Err(_) => None,
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
    match detect_vcs(path).or_else(|_| detect_parent_vcs(path)) {
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

/// The one `git` constructor in this crate; every call builds on it.
///
/// Repository-controlled code never runs inside Plak's own git steps: the
/// inherited `GIT_*` steering variables are dropped (a caller re-adds only the
/// ones it sets deliberately), hooks resolve under `/dev/null` and so never exist,
/// and the fsmonitor daemon hook is off. `git` never prompts: no terminal
/// prompt and no askpass helper, so a remote or credential question fails
/// instead of waiting for input nobody can give.
fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    strip_git_environment(&mut command);
    command
        .current_dir(cwd)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "");
    // Without this, an unconfigured identity makes git resolve the host's
    // FQDN for every reflog entry; musl's resolver waits out its full timeout
    // when there is no network. Configured `user.email` still takes precedence.
    if std::env::var_os("EMAIL").is_none() {
        command.env("EMAIL", FALLBACK_EMAIL);
    }
    command
}

/// Identity fallback for reflog entries Plak's git steps write when neither
/// git configuration nor the environment names one.
const FALLBACK_EMAIL: &str = "plak@local";

/// Every variable git reads to relocate the repository, inject configuration,
/// run an external program, or change identity, refs, attributes or tracing.
/// A fixed list because the CLI boundary forbids ambient environment
/// iteration; removing `GIT_CONFIG_COUNT` leaves any `GIT_CONFIG_KEY_<n>` inert.
const STEERING_GIT_VARIABLES: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_ASKPASS",
    "GIT_ATTR_NOSYSTEM",
    "GIT_ATTR_SOURCE",
    "GIT_AUTHOR_DATE",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_NAME",
    "GIT_CEILING_DIRECTORIES",
    "GIT_COMMITTER_DATE",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
    "GIT_DEFAULT_HASH",
    "GIT_DEFAULT_REF_FORMAT",
    "GIT_DIFF_OPTS",
    "GIT_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_EDITOR",
    "GIT_EXEC_PATH",
    "GIT_EXTERNAL_DIFF",
    "GIT_GLOB_PATHSPECS",
    "GIT_GRAFT_FILE",
    "GIT_ICASE_PATHSPECS",
    "GIT_INDEX_FILE",
    "GIT_INDEX_VERSION",
    "GIT_LITERAL_PATHSPECS",
    "GIT_NAMESPACE",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_OPTIONAL_LOCKS",
    "GIT_PAGER",
    "GIT_PREFIX",
    "GIT_QUARANTINE_PATH",
    "GIT_REPLACE_REF_BASE",
    "GIT_SEQUENCE_EDITOR",
    "GIT_SHALLOW_FILE",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_TEMPLATE_DIR",
    "GIT_TERMINAL_PROMPT",
    "GIT_TRACE",
    "GIT_TRACE2",
    "GIT_TRACE2_EVENT",
    "GIT_TRACE2_PERF",
    "GIT_TRACE_PACKET",
    "GIT_TRACE_PERFORMANCE",
    "GIT_TRACE_SETUP",
    "GIT_WORK_TREE",
];

fn strip_git_environment(command: &mut Command) {
    for name in STEERING_GIT_VARIABLES {
        command.env_remove(name);
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

/// A `git` step that changes a copy, bounded by [`GIT_WRITE_TIMEOUT`].
fn git_run(cwd: &Path, args: &[&str]) -> Result<Output, std::io::Error> {
    if !GIT_CLI_ENABLED {
        return Err(std::io::Error::other("Git CLI backend not included"));
    }
    let mut command = git_command(cwd);
    command.args(args);
    output_within(command, GIT_WRITE_TIMEOUT)
}

/// A `git` read, bounded by [`GIT_QUERY_TIMEOUT`]. It takes no optional locks,
/// so a status poll never contends with the agent's own `git`.
pub(crate) fn git_query(cwd: &Path, args: &[&str]) -> Result<Output, std::io::Error> {
    if !GIT_CLI_ENABLED {
        return Err(std::io::Error::other("Git CLI backend not included"));
    }
    let mut command = git_command(cwd);
    command.args(args).env("GIT_OPTIONAL_LOCKS", "0");
    output_within(command, GIT_QUERY_TIMEOUT)
}

/// `git` with the snapshot's temp index bound. Every index-writing step of
/// `snapshot` runs through here so the real index is never touched.
fn git_run_indexed(cwd: &Path, index: &Path, args: &[&str]) -> Result<Output, std::io::Error> {
    let mut command = git_command(cwd);
    command.args(args).env("GIT_INDEX_FILE", index);
    output_within(command, GIT_WRITE_TIMEOUT)
}

fn git_text(cwd: &Path, args: &[&str], what: &str) -> Result<String, VcsError> {
    let output = git_query(cwd, args).map_err(|_| mint_error(what))?;
    if !output.status.success() {
        return Err(mint_error(what));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Fixed author/committer date for snapshot wrapper commits.
const SNAPSHOT_DATE: &str = "@0 +0000";

fn mint_error(what: &str) -> VcsError {
    VcsError::mint_unavailable(what)
}

fn git_branch(cwd: &Path) -> Option<String> {
    let output = git_query(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|branch| !branch.is_empty())
}

fn git_snapshot(path: &Path, label: &str) -> Result<VcsSnapshot, VcsError> {
    let reported = git_text(
        path,
        &["rev-parse", "--git-path", "index"],
        "git index path could not be resolved",
    )?;
    if reported.is_empty() {
        return Err(mint_error("git index path was empty"));
    }
    let index_source = {
        let reported = Path::new(&reported);
        if reported.is_absolute() {
            reported.to_path_buf()
        } else {
            path.join(reported)
        }
    };
    let scratch = private_scratch().map_err(|_| mint_error("snapshot scratch was unavailable"))?;
    let temp_index = scratch.path().join("index");
    // A missing source index is an empty index; a zero-byte file is not a
    // valid index, so leave the temp path absent rather than creating one.
    if index_source.exists() {
        let seeded = std::fs::File::open(&index_source)
            .and_then(|mut source| std::io::copy(&mut source, &mut create_private(&temp_index)?));
        if seeded.is_err() {
            return Err(mint_error("snapshot index could not be seeded"));
        }
    }
    git_snapshot_indexed(path, &temp_index, label)
}

fn git_snapshot_indexed(
    path: &Path,
    temp_index: &Path,
    label: &str,
) -> Result<VcsSnapshot, VcsError> {
    let staged = git_run_indexed(path, temp_index, &["add", "-A"])
        .map_err(|_| mint_error("git add could not stage the snapshot"))?;
    if !staged.status.success() {
        return Err(mint_error("git add refused the snapshot"));
    }
    unstage_secret_paths(path, temp_index)?;
    let output = git_run_indexed(path, temp_index, &["write-tree"])
        .map_err(|_| mint_error("git write-tree failed"))?;
    if !output.status.success() {
        return Err(mint_error("git write-tree refused the snapshot"));
    }
    let content = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if content.is_empty() {
        return Err(mint_error("git write-tree produced no tree"));
    }
    let message = format!("plak snapshot {label}");
    let head = git_run(path, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|head| !head.is_empty());
    let mut args = vec![
        "-c",
        "user.name=plak",
        "-c",
        "user.email=plak@local",
        // A fixed date keeps the wrapper commit a pure function of the tree,
        // HEAD, and the label: no wall clock in a candidate's identity.
        "-c",
        "commit.gpgsign=false",
        "commit-tree",
        content.as_str(),
    ];
    if let Some(head) = head.as_deref() {
        args.push("-p");
        args.push(head);
    }
    args.push("-m");
    args.push(message.as_str());
    let mut command = git_command(path);
    command
        .args(&args)
        .env("GIT_AUTHOR_DATE", SNAPSHOT_DATE)
        .env("GIT_COMMITTER_DATE", SNAPSHOT_DATE);
    let output = output_within(command, GIT_WRITE_TIMEOUT)
        .map_err(|_| mint_error("git commit-tree failed"))?;
    if !output.status.success() {
        return Err(mint_error("git commit-tree refused the snapshot"));
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if commit.is_empty() {
        return Err(mint_error("git commit-tree produced no commit"));
    }
    Ok(VcsSnapshot {
        history_left_base: false,
        commit,
        content,
        change_id: None,
    })
}

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
fn jj_detached_copy(path: &Path) -> Option<PathBuf> {
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
fn jj_mint_lock(launch: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(map.entry(launch.to_path_buf()).or_default())
}

fn jj_snapshot(path: &Path) -> Result<VcsSnapshot, VcsError> {
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

fn mint_git_worktree(
    launch: &Path,
    agent_id: &str,
    parent_cwd: &Path,
    mode: WorkspaceMode,
    over: Option<&VcsSnapshot>,
) -> Result<MintedCopy, VcsError> {
    let dest = launch.join(".plak/worktrees").join(agent_id);
    if dest.exists() || dest.is_symlink() {
        reclaim_orphan_worktree(launch, &dest, agent_id)?;
    }
    std::fs::create_dir_all(dest.parent().unwrap_or(launch))
        .map_err(|_| mint_error("could not create .plak/worktrees"))?;
    ensure_plak_mint_excludes(launch)?;
    // The base is the named candidate, or the parent's state at this instant.
    // Never the launch HEAD.
    let base = match over {
        Some(candidate) => candidate.clone(),
        None => git_snapshot(parent_cwd, agent_id)?,
    };
    let branch = (mode == WorkspaceMode::Fork).then(|| format!("plak/{agent_id}"));
    let dest_text = dest.display().to_string();
    let mut args = vec!["worktree", "add"];
    match branch.as_deref() {
        // `-B`: a `plak/<id>` branch left behind by an earlier tree in this
        // checkout (its worktree gone) is reset onto the new base instead of
        // refusing the mint. A branch still checked out elsewhere is refused
        // by git itself. The branch never holds work: seals pin candidates
        // under their own refs and never advance it.
        Some(branch) => {
            args.push("-B");
            args.push(branch);
        }
        // an `execute` copy is a read-only source that is thrown
        // away unless something seals and adopts it, so it takes no branch.
        None => args.push("--detach"),
    }
    args.push(dest_text.as_str());
    args.push(base.commit.as_str());
    let cleanup_branch = branch.clone().unwrap_or_default();
    let output =
        git_run(launch, &args).map_err(|_| VcsError::mint_failed("git worktree add failed"))?;
    if !output.status.success() {
        unmint_git_dest(launch, &dest, dest_text.as_str(), cleanup_branch.as_str());
        return Err(VcsError::mint_failed("git worktree add refused the mint"));
    }
    if let Err(error) = crate::git_redirect_fence::install(&dest) {
        unmint_git_dest(launch, &dest, dest_text.as_str(), cleanup_branch.as_str());
        return Err(mint_error(&format!(
            "git-redirect fence could not be installed: {error}"
        )));
    }
    Ok(MintedCopy {
        path: dest,
        branch,
        base,
    })
}

fn mint_jj_workspace(
    launch: &Path,
    agent_id: &str,
    parent_cwd: &Path,
    over: Option<&VcsSnapshot>,
) -> Result<MintedCopy, VcsError> {
    let lock = jj_mint_lock(launch);
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::native_jj::mint(launch, agent_id, parent_cwd, over)
}

fn git_adopt(
    target_path: &Path,
    base_commit: &str,
    candidate_commit: &str,
    expected_target: Option<&str>,
) -> Result<AdoptLanding, AdoptFailure> {
    let target = git_snapshot(target_path, "adopt-target")
        .map_err(|error| AdoptFailure::Unavailable(error.detail))?;
    if let Some(expected) = expected_target
        && !target.matches_expectation(expected)
    {
        return Err(AdoptFailure::TargetChanged(target.token()));
    }
    let merged = git_run(
        target_path,
        &[
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={base_commit}"),
            target.commit.as_str(),
            candidate_commit,
        ],
    )
    .map_err(|_| AdoptFailure::Unavailable("git merge-tree failed".to_owned()))?;
    let stdout = String::from_utf8_lossy(&merged.stdout).into_owned();
    if !merged.status.success() {
        // `merge-tree --write-tree` exits 1 for a content conflict and >1 for
        // anything else (bad base object, corrupt ref). Only the former is a
        // `merge_conflict`; the rest is the VCS refusing to run the merge.
        if merged.status.code() == Some(1) {
            return Err(AdoptFailure::Conflict(parse_merge_tree_conflicts(&stdout)));
        }
        return Err(AdoptFailure::Unavailable(format!(
            "git merge-tree exited {}: {}",
            merged
                .status
                .code()
                .map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
            stderr_excerpt(&merged.stderr)
        )));
    }
    let Some(merged_tree) = stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Err(AdoptFailure::Unavailable(
            "git merge-tree produced no tree".to_owned(),
        ));
    };
    let names = git_run(
        target_path,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            target.content.as_str(),
            merged_tree,
        ],
    )
    .map_err(|_| AdoptFailure::Unavailable("git diff failed".to_owned()))?;
    if !names.status.success() {
        return Err(AdoptFailure::Unavailable(
            "git diff refused the merged tree".to_owned(),
        ));
    }
    let files_changed = u32::try_from(
        String::from_utf8_lossy(&names.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
    )
    .unwrap_or(u32::MAX);
    if files_changed == 0 {
        return Ok(AdoptLanding {
            target_before: target.commit.clone(),
            target_after: target.commit,
            files_changed: 0,
            child_base: None,
        });
    }
    let patch = git_run(
        target_path,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "-M",
            target.content.as_str(),
            merged_tree,
        ],
    )
    .map_err(|_| AdoptFailure::Unavailable("git diff failed".to_owned()))?;
    if !patch.status.success() {
        return Err(AdoptFailure::Unavailable(
            "git diff refused the merged tree".to_owned(),
        ));
    }
    // `git apply` is atomic by default, worktree-only, binary-safe, and never
    // commits — the human checkout keeps its own HEAD and its own index.
    if let Err(stderr) = git_apply(target_path, &patch.stdout) {
        // The patch was cut against the target snapshot taken above; a hunk
        // that no longer applies means the worktree moved in between. Any
        // other refusal (mode, CRLF, permissions) is the VCS, not a race.
        let moved = [
            "does not match index",
            "patch does not apply",
            "already exists",
        ]
        .iter()
        .any(|needle| stderr.contains(needle));
        if moved {
            return Err(AdoptFailure::TargetChanged(target.token()));
        }
        return Err(AdoptFailure::Unavailable(format!(
            "git apply refused the merged patch: {stderr}"
        )));
    }
    let after = git_snapshot(target_path, "adopt-after")
        .map_err(|error| AdoptFailure::Unavailable(error.detail))?;
    Ok(AdoptLanding {
        target_before: target.commit,
        target_after: after.commit,
        files_changed,
        child_base: None,
    })
}

/// Applies the merged-tree patch to the target's working copy. The patch goes
/// through a scratch file rather than a pipe so this module keeps one process
/// constructor per VCS. `Err` carries git's stderr excerpt.
fn git_apply(target_path: &Path, patch: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let Ok(scratch) = private_scratch() else {
        return Err("the patch scratch directory could not be created".to_owned());
    };
    let patch_path = scratch.path().join("adopt.patch");
    if create_private(&patch_path)
        .and_then(|mut file| file.write_all(patch))
        .is_err()
    {
        return Err("the patch scratch file could not be written".to_owned());
    }
    let patch_text = patch_path.display().to_string();
    // `git apply` is atomic by default, worktree-only, and binary-safe.
    let applied = git_run(target_path, &["apply", patch_text.as_str()]);
    drop(scratch);
    match applied {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(stderr_excerpt(&output.stderr)),
        Err(error) => Err(format!("git apply could not start: {error}")),
    }
}

/// How much subprocess stderr a typed failure quotes.
const STDERR_EXCERPT_BYTES: usize = 240;

/// The first `STDERR_EXCERPT_BYTES` of a subprocess's stderr, one line.
fn stderr_excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let joined: String = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if joined.is_empty() {
        return "(no stderr)".to_owned();
    }
    let mut end = joined.len().min(STDERR_EXCERPT_BYTES);
    while end > 0 && !joined.is_char_boundary(end) {
        end -= 1;
    }
    joined[..end].to_owned()
}

/// `git merge-tree --write-tree` prints the tree, then a conflicted-file info
/// section of `<mode> <object> <stage>\t<path>` rows, then the messages.
fn parse_merge_tree_conflicts(stdout: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in stdout.lines().skip(1) {
        if line.trim().is_empty() {
            break;
        }
        let Some((_, path)) = line.rsplit_once('\t') else {
            continue;
        };
        let path = path.trim().to_owned();
        if !path.is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

fn jj_adopt(
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

fn git_worktree_dirty(path: &Path) -> bool {
    git_query(path, &["status", "--porcelain", "-uall"])
        .map(|output| output.status.success() && !output.stdout.is_empty())
        .unwrap_or(false)
}

fn jj_workspace_dirty(path: &Path) -> bool {
    crate::native_jj::dirty(path)
}

fn git_ahead_behind(child: &Path, parent: &Path) -> Option<(u64, u64)> {
    let parent_head = git_query(parent, &["rev-parse", "HEAD"]).ok()?;
    if !parent_head.status.success() {
        return None;
    }
    let parent_sha = String::from_utf8_lossy(&parent_head.stdout)
        .trim()
        .to_owned();
    if parent_sha.is_empty() {
        return None;
    }
    let counted = git_query(
        child,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{parent_sha}"),
        ],
    )
    .ok()?;
    if !counted.status.success() {
        return None;
    }
    parse_left_right_counts(&String::from_utf8_lossy(&counted.stdout))
}

fn jj_ahead_behind(child: &Path, parent: &Path, base: Option<&str>) -> Option<(u64, u64)> {
    crate::native_jj::ahead_behind(child, parent, base)
}

fn parse_left_right_counts(text: &str) -> Option<(u64, u64)> {
    let mut parts = text.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

const SECRET_NAMES: &[&str] = &[
    ".netrc",
    ".npmrc",
    "credentials.json",
    "id_rsa",
    "id_ed25519",
];
const SECRET_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx"];

pub(super) fn secret_fileset() -> String {
    let mut patterns = vec![
        "root-glob-i:'**/.env'".to_owned(),
        "(root-glob-i:'**/.env.*' ~ root-glob-i:'**/.env.example')".to_owned(),
    ];
    patterns.extend(
        SECRET_NAMES
            .iter()
            .map(|name| format!("root-glob-i:'**/{name}'")),
    );
    patterns.extend(
        SECRET_EXTENSIONS
            .iter()
            .map(|ext| format!("root-glob-i:'**/*.{ext}'")),
    );
    patterns.join(" | ")
}

pub(super) fn is_secret_git_path(path: &str) -> bool {
    let file = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    let lower = file.to_ascii_lowercase();
    lower == ".env"
        || (lower.starts_with(".env.") && lower != ".env.example")
        || SECRET_NAMES.contains(&lower.as_str())
        || lower
            .rsplit_once('.')
            .is_some_and(|(_, ext)| SECRET_EXTENSIONS.contains(&ext))
}

/// Keeps a snapshot commit from pinning a secret the tool policy already
/// excludes. Runs against the snapshot's temp index, never the
/// real one.
fn unstage_secret_paths(child: &Path, index: &Path) -> Result<(), VcsError> {
    let listed = git_run_indexed(child, index, &["diff", "--cached", "--name-only"])
        .map_err(|_| mint_error("git diff --cached failed on the snapshot index"))?;
    let listed = if listed.status.success() {
        String::from_utf8_lossy(&listed.stdout).into_owned()
    } else {
        // No HEAD to diff against: every staged path is new.
        let all = git_run_indexed(child, index, &["ls-files", "--cached"])
            .map_err(|_| mint_error("git ls-files failed on the snapshot index"))?;
        if !all.status.success() {
            return Err(mint_error("snapshot index could not be listed"));
        }
        String::from_utf8_lossy(&all.stdout).into_owned()
    };
    for path in listed.lines() {
        let path = path.trim();
        if path.is_empty() || !is_secret_git_path(path) {
            continue;
        }
        let reset = git_run_indexed(child, index, &["reset", "-q", "--", path])
            .map_err(|_| mint_error("git reset failed on the snapshot index"))?;
        if reset.status.success() {
            continue;
        }
        // Unborn HEAD: there is nothing to reset the entry back to.
        let removed = git_run_indexed(
            child,
            index,
            &["rm", "--cached", "--force", "-q", "--", path],
        )
        .map_err(|_| mint_error("git rm --cached failed on the snapshot index"))?;
        if !removed.status.success() {
            return Err(mint_error("a secret path could not be unstaged"));
        }
    }
    Ok(())
}

fn ensure_plak_mint_excludes(launch: &Path) -> Result<(), VcsError> {
    let git = launch.join(".git");
    if !git.exists() {
        return Ok(());
    }
    let reported = git_text(
        launch,
        &["rev-parse", "--git-path", "info/exclude"],
        "git exclude path could not be resolved",
    )?;
    if reported.is_empty() {
        return Err(mint_error("git exclude path was empty"));
    }
    let path = {
        let reported = Path::new(&reported);
        if reported.is_absolute() {
            reported.to_path_buf()
        } else {
            launch.join(reported)
        }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| mint_error("git exclude directory could not be created"))?;
    }
    append_exclude_lines(&path)?;
    Ok(())
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

/// Removes a copy a crash left between `git worktree add` and the tree
/// recording it. Copy ids are tree-scoped and the tree never mints an id it has
/// recorded, so an existing destination for this id was never handed to an
/// agent: its registration, directory and `plak/<copy_id>` branch all go.
fn reclaim_orphan_worktree(launch: &Path, dest: &Path, copy_id: &str) -> Result<(), VcsError> {
    let dest_text = dest.display().to_string();
    let _ = git_run(
        launch,
        &["worktree", "remove", "--force", dest_text.as_str()],
    );
    if dest.is_symlink() {
        std::fs::remove_file(dest)
    } else if dest.exists() {
        std::fs::remove_dir_all(dest)
    } else {
        Ok(())
    }
    .map_err(|_| VcsError::mint_failed("an orphaned worktree directory could not be removed"))?;
    let pruned = git_run(launch, &["worktree", "prune"])
        .map_err(|_| VcsError::mint_failed("git worktree prune could not run"))?;
    if !pruned.status.success() {
        return Err(VcsError::mint_failed("git worktree prune refused"));
    }
    let branch = format!("plak/{copy_id}");
    let reference = format!("refs/heads/{branch}");
    let present = git_run(
        launch,
        &["show-ref", "--verify", "--quiet", reference.as_str()],
    )
    .map_err(|_| VcsError::mint_failed("orphan branch state could not be checked"))?;
    if present.status.success() {
        let removed = git_run(launch, &["branch", "-D", branch.as_str()])
            .map_err(|_| VcsError::mint_failed("orphan branch cleanup could not run"))?;
        if !removed.status.success() {
            return Err(VcsError::mint_failed("orphan branch cleanup failed"));
        }
    }
    Ok(())
}

fn unmint_git_dest(launch: &Path, dest: &Path, dest_text: &str, branch: &str) {
    let _ = git_run(launch, &["worktree", "remove", "--force", dest_text]);
    let _ = std::fs::remove_dir_all(dest);
    if !branch.is_empty() {
        // Safe even with an unadopted candidate: the candidate's commit is
        // pinned under `refs/plak/<namespace>/candidates/<id>`, which is never removed here.
        let _ = git_run(launch, &["branch", "-D", branch]);
    }
}

/// A recording [`VcsOps`] double for tree tests: no subprocess, no repo. Every
/// call is logged. Minted copies are plain directories under
/// `<launch>/.plak/fake/<agent_id>`; seals diverge by default, and a seal, an
/// adopt, or a hook can be pointed at an explicit answer for one call.
/// Compiled for this crate's tests and, under the `test-support` feature,
/// for the crates the topology moves into at the cut.
#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, PoisonError};

    use super::{AdoptFailure, AdoptLanding, MintedCopy, VcsError, VcsOps, VcsSnapshot};
    use plakous_host_protocol::topology::WorkspaceMode;

    /// Which [`VcsOps`] method one recorded call was.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum FakeOp {
        /// [`VcsOps::mint_from`].
        Mint,
        /// [`VcsOps::seal`].
        Seal,
        /// [`VcsOps::unpin`].
        Unpin,
        /// [`VcsOps::adopt`].
        Adopt,
        /// [`VcsOps::run_mint_hook`].
        Hook,
        /// [`VcsOps::unmint`].
        Unmint,
        /// [`VcsOps::dirty`].
        Dirty,
        /// [`VcsOps::ahead_behind`].
        AheadBehind,
        /// [`VcsOps::label`].
        Label,
        /// [`VcsOps::diverged_from_base`].
        Diverged,
        /// [`VcsOps::copy_is_over_candidate`].
        OverCandidate,
    }

    impl FakeOp {
        /// Every operation this port offers may perform repository or filesystem work, and a caller
        /// that owns a lock must run none of them under it.
        /// Kept as a predicate so a caller's oracle reads as a rule rather
        /// than as a hard-coded set.
        #[must_use]
        pub const fn is_effect(self) -> bool {
            true
        }
    }

    /// One recorded call: what it was, and whether the caller's lock probe
    /// said its lock was held at the time.
    #[derive(Clone, Debug)]
    pub struct FakeCall {
        /// Which method was called.
        pub op: FakeOp,
        /// What the installed lock probe answered when the call began.
        pub lock_held: bool,
    }

    type LockProbe = Box<dyn Fn() -> bool + Send + Sync>;

    /// A [`VcsOps`] that records every call instead of running it: copies are
    /// plain directories under `<launch>/.plak/fake/<agent_id>`.
    #[derive(Default)]
    pub struct FakeVcs {
        calls: Mutex<Vec<FakeCall>>,
        lock_probe: Mutex<Option<LockProbe>>,
        next_seal: Mutex<Option<Result<VcsSnapshot, VcsError>>>,
        next_adopt: Mutex<Option<Result<AdoptLanding, AdoptFailure>>>,
        next_hook: Mutex<Option<Result<(), VcsError>>>,
        next_unmint: Mutex<Option<Result<(), VcsError>>>,
        next_unpin: Mutex<Option<Result<(), VcsError>>>,
        label: Mutex<Option<String>>,
    }

    impl FakeVcs {
        /// A recorder with no answers pinned and a fixed presentation label.
        #[must_use]
        pub fn new() -> Self {
            Self {
                label: Mutex::new(Some("fake-main".to_owned())),
                ..Self::default()
            }
        }

        /// Installs the probe every call consults: a tree test hands in
        /// "is the tree mutex held right now".
        pub fn watch_lock(&self, probe: impl Fn() -> bool + Send + Sync + 'static) {
            *self
                .lock_probe
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(probe));
        }

        /// Every call so far, in order.
        #[must_use]
        pub fn calls(&self) -> Vec<FakeCall> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// Every call of one kind, in order.
        #[must_use]
        pub fn calls_of(&self, op: FakeOp) -> Vec<FakeCall> {
            self.calls()
                .into_iter()
                .filter(|call| call.op == op)
                .collect()
        }

        /// The next `seal` answers with this instead of a diverging snapshot.
        pub fn seal_next_with(&self, answer: Result<VcsSnapshot, VcsError>) {
            *self
                .next_seal
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(answer);
        }

        /// The next `adopt` answers with this instead of landing cleanly.
        pub fn adopt_next_with(&self, answer: Result<AdoptLanding, AdoptFailure>) {
            *self
                .next_adopt
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(answer);
        }

        /// The next `on_mint` hook answers with this instead of succeeding.
        pub fn hook_next_with(&self, answer: Result<(), VcsError>) {
            *self
                .next_hook
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(answer);
        }

        /// Injects the next workspace cleanup result.
        pub fn unmint_next_with(&self, answer: Result<(), VcsError>) {
            *self
                .next_unmint
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(answer);
        }
        /// Injects the next candidate-reference cleanup result.
        pub fn unpin_next_with(&self, answer: Result<(), VcsError>) {
            *self
                .next_unpin
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(answer);
        }

        fn record(&self, op: FakeOp, _path: &Path) {
            let lock_held = self
                .lock_probe
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .is_some_and(|probe| probe());
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(FakeCall { op, lock_held });
        }

        fn copy_dir(launch: &Path, agent_id: &str) -> PathBuf {
            launch.join(".plak").join("fake").join(agent_id)
        }

        /// The base a copy of `agent_id` is minted on.
        #[must_use]
        pub fn base_for(agent_id: &str) -> VcsSnapshot {
            VcsSnapshot {
                history_left_base: false,
                commit: format!("base-{agent_id}"),
                content: format!("content-base-{agent_id}"),
                change_id: None,
            }
        }
    }

    impl VcsOps for FakeVcs {
        fn mint_from(
            &self,
            launch: &Path,
            agent_id: &str,
            _copy_id: &str,
            _parent_cwd: &Path,
            mode: WorkspaceMode,
            base: Option<&VcsSnapshot>,
        ) -> Result<MintedCopy, VcsError> {
            let path = Self::copy_dir(launch, agent_id);
            self.record(FakeOp::Mint, &path);
            std::fs::create_dir_all(&path)
                .map_err(|error| VcsError::mint_failed(format!("fake mint: {error}")))?;
            Ok(MintedCopy {
                path,
                branch: (mode == WorkspaceMode::Fork).then(|| format!("plak/{agent_id}")),
                base: base.cloned().unwrap_or_else(|| Self::base_for(agent_id)),
            })
        }

        fn seal(
            &self,
            child_path: &Path,
            _namespace: &str,
            candidate_id: &str,
        ) -> Result<VcsSnapshot, VcsError> {
            self.record(FakeOp::Seal, child_path);
            if let Some(answer) = self
                .next_seal
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                return answer;
            }
            Ok(VcsSnapshot {
                history_left_base: false,
                commit: format!("sealed-{candidate_id}"),
                content: format!("content-{candidate_id}"),
                change_id: None,
            })
        }

        fn unpin(
            &self,
            child_path: &Path,
            _namespace: &str,
            _candidate_id: &str,
        ) -> Result<(), VcsError> {
            self.record(FakeOp::Unpin, child_path);
            self.next_unpin
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .unwrap_or(Ok(()))
        }

        fn adopt(
            &self,
            target_path: &Path,
            _child_path: &Path,
            _base_commit: &str,
            _base_change_id: Option<&str>,
            candidate: &VcsSnapshot,
            _expected_target: Option<&str>,
        ) -> Result<AdoptLanding, AdoptFailure> {
            self.record(FakeOp::Adopt, target_path);
            if let Some(answer) = self
                .next_adopt
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                return answer;
            }
            Ok(AdoptLanding {
                target_before: "target-before".to_owned(),
                target_after: candidate.commit.clone(),
                files_changed: 1,
                child_base: None,
            })
        }

        fn run_mint_hook(
            &self,
            cwd: &Path,
            _command: &str,
            _env: &BTreeMap<String, String>,
        ) -> Result<(), VcsError> {
            self.record(FakeOp::Hook, cwd);
            self.next_hook
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .unwrap_or(Ok(()))
        }

        fn unmint(&self, _launch: &Path, path: &Path) -> Result<(), VcsError> {
            self.record(FakeOp::Unmint, path);
            if let Some(answer) = self
                .next_unmint
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                answer?;
            }
            std::fs::remove_dir_all(path)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(|error| {
                    VcsError::new(
                        super::VcsErrorKind::RecoveryRequired,
                        format!("fake unmint: {error}"),
                    )
                })
        }

        fn dirty(&self, path: &Path) -> bool {
            self.record(FakeOp::Dirty, path);
            false
        }

        fn ahead_behind(
            &self,
            child: &Path,
            _parent: &Path,
            _base: Option<&str>,
        ) -> Option<(u64, u64)> {
            self.record(FakeOp::AheadBehind, child);
            None
        }

        fn label(&self, path: &Path) -> Option<String> {
            self.record(FakeOp::Label, path);
            self.label
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn diverged_from_base(
            &self,
            path: &Path,
            _base_change_id: Option<&str>,
            _base_commit: &str,
        ) -> Result<bool, VcsError> {
            self.record(FakeOp::Diverged, path);
            Ok(true)
        }

        fn copy_is_over_candidate(
            &self,
            launch: &Path,
            _agent_id: &str,
            _candidate: &VcsSnapshot,
        ) -> bool {
            self.record(FakeOp::OverCandidate, launch);
            false
        }
    }
}

/// Refuses a JJ copy whose recorded mint base is no longer its strict ancestor.
pub fn validate_jj_copy_base(path: &Path, base: &str) -> Result<(), VcsError> {
    crate::native_jj::validate_copy_base(path, base)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn git_repo() -> tempfile::TempDir {
        git_repo_with_init(&["init", "-b", "main"])
    }

    fn git_repo_with_init(init: &[&str]) -> tempfile::TempDir {
        let root = tempdir().unwrap();
        for args in [
            init.to_vec(),
            vec!["config", "user.email", "plak@example.test"],
            vec!["config", "user.name", "plak"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(root.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
        for args in [vec!["add", "SEED.txt"], vec!["commit", "-m", "seed"]] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(root.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        root
    }

    fn jj_available() -> bool {
        if !cfg!(feature = "jj-lib") {
            return false;
        }
        Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn jj_repo() -> tempfile::TempDir {
        jj_repo_with_init_args(&["git", "init"])
    }

    fn jj_repo_with_init_args(init_args: &[&str]) -> tempfile::TempDir {
        let root = tempdir().unwrap();
        for args in [init_args, &["commit", "-m", "seed"]] {
            if args[0] == "commit" {
                std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
            }
            assert!(
                Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
                    .args(["--no-pager"])
                    .args([
                        "--config",
                        "user.name=plak",
                        "--config",
                        "user.email=plak@local",
                    ])
                    .args(args)
                    .env("JJ_CONFIG", "/dev/null")
                    .current_dir(root.path())
                    .status()
                    .unwrap()
                    .success(),
                "jj {args:?} failed"
            );
        }
        root
    }

    /// With no configured identity, git derives a default email from the
    /// host's FQDN; under musl without a network that lookup stalls every
    /// reflog write for the resolver timeout. Plak's steps supply a fallback.
    #[test]
    fn ref_writes_without_a_configured_identity_use_the_fallback_email() {
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let isolated = |args: &[&str]| {
            let mut command = git_command(root.path());
            command
                .args(args)
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path())
                .env("GIT_CONFIG_NOSYSTEM", "1");
            assert!(command.status().unwrap().success(), "git {args:?}");
        };
        isolated(&["init", "-q", "-b", "main"]);
        isolated(&[
            "-c",
            "user.name=seed",
            "-c",
            "user.email=seed@local",
            "commit",
            "--allow-empty",
            "-qm",
            "seed",
        ]);
        isolated(&["update-ref", "-m", "probe", "refs/heads/probe", "HEAD"]);
        let reflog =
            std::fs::read_to_string(root.path().join(".git/logs/refs/heads/probe")).unwrap();
        let expected = std::env::var("EMAIL").unwrap_or_else(|_| FALLBACK_EMAIL.to_owned());
        assert!(reflog.contains(&format!("<{expected}>")), "{reflog}");
    }

    /// A `plak/c1` branch an earlier tree left behind does not block a new
    /// `c1` fork.
    #[test]
    fn a_stale_plak_branch_does_not_block_a_fork_mint() {
        let repo = git_repo();
        assert!(
            Command::new("git")
                .args(["branch", "plak/c1"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        let minted = SystemVcs
            .mint_from(
                repo.path(),
                "c1",
                "c1",
                repo.path(),
                WorkspaceMode::Fork,
                None,
            )
            .unwrap();
        assert_eq!(minted.branch.as_deref(), Some("plak/c1"));
        assert!(minted.path.join("SEED.txt").is_file());
    }

    /// A crashed first tree can retry cleanup after its `c1` was removed and
    /// a second tree minted its own `c1`. Tree-scoped copy ids make those
    /// physical resources distinct, so the stale retry cannot remove the
    /// second tree's worktree or branch.
    #[test]
    fn stale_tree_cleanup_cannot_remove_another_trees_reused_child_id() {
        let repo = git_repo();
        let first_id = format!("{}-c1", digest_text("tree-first"));
        let second_id = format!("{}-c1", digest_text("tree-second"));
        let first = SystemVcs
            .mint_from(
                repo.path(),
                &first_id,
                &first_id,
                repo.path(),
                WorkspaceMode::Fork,
                None,
            )
            .unwrap();
        SystemVcs.unmint(repo.path(), &first.path).unwrap();

        let second = SystemVcs
            .mint_from(
                repo.path(),
                &second_id,
                &second_id,
                repo.path(),
                WorkspaceMode::Fork,
                None,
            )
            .unwrap();
        assert_ne!(first.path, second.path);
        // A stale repeat of the first tree's cleanup finds its copy already
        // gone and succeeds without touching the second tree's copy.
        SystemVcs.unmint(repo.path(), &first.path).unwrap();
        assert!(second.path.join("SEED.txt").is_file());
        let branch = second.branch.as_deref().unwrap();
        assert!(
            git_run(
                repo.path(),
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}")
                ]
            )
            .unwrap()
            .status
            .success()
        );
    }

    /// Concurrent mints in one jj repo must each leave a
    /// *workspace* of the launch repo — `.jj/repo` a pointer file — never a
    /// standalone repo, and none of them may be refused because a sibling was
    /// adding a workspace at the same instant.
    #[test]
    fn concurrent_jj_mints_all_yield_workspace_pointer_copies() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let launch = repo.path().to_path_buf();
        let minted: Vec<_> = ["c1", "c2", "c3", "c4"]
            .into_iter()
            .map(|agent_id| {
                let launch = launch.clone();
                std::thread::spawn(move || {
                    mint_worktree_from(&launch, agent_id, &launch, WorkspaceMode::Fork, None)
                        .map(|copy| copy.path)
                })
            })
            .collect();
        for handle in minted {
            let path = handle
                .join()
                .unwrap()
                .unwrap_or_else(|error| panic!("concurrent mint refused: {}", error.detail));
            let pointer = path.join(".jj").join("repo");
            assert!(
                pointer.is_file(),
                "{} is not a workspace pointer of the launch repo",
                pointer.display()
            );
            assert!(jj_detached_copy(&path).is_none());
            // The copy is a workspace, so the launch repo can read it.
            assert!(jj_snapshot(&path).is_ok());
        }
    }

    /// A copy whose `.jj/repo` is a *directory* is a
    /// standalone repo, not a workspace of the launch repo. Every id it
    /// reports is foreign, so it is refused by name rather than snapshotted,
    /// compared against a base, or sealed as diverged.
    #[test]
    fn a_standalone_jj_repo_in_a_minted_copy_is_refused_by_name() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let launch = repo.path().to_path_buf();
        let copy = mint_worktree_from(&launch, "c1", &launch, WorkspaceMode::Fork, None).unwrap();
        let base = copy.base.clone();
        std::fs::rename(copy.path.join(".jj"), copy.path.join(".jj.bak")).unwrap();
        assert!(
            Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
                .args(["--no-pager", "git", "init"])
                .args([
                    "--config",
                    "user.name=plak",
                    "--config",
                    "user.email=plak@local",
                ])
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(&copy.path)
                .status()
                .unwrap()
                .success()
        );
        assert!(copy.path.join(".jj").join("repo").is_dir());
        let error = jj_snapshot(&copy.path).unwrap_err();
        assert_eq!(error.kind, VcsErrorKind::MintUnavailable);
        assert!(
            error.detail.contains("not a workspace of the launch repo")
                && error.detail.contains("c1"),
            "{}",
            error.detail
        );
        let error =
            jj_diverged_from_base(&copy.path, base.change_id.as_deref(), base.commit.as_str())
                .unwrap_err();
        assert!(
            error.detail.contains("standalone jj repo"),
            "{}",
            error.detail
        );
    }

    #[test]
    fn jj_candidate_resolution_uses_immutable_commit_identity() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let saved = jj_snapshot(repo.path()).unwrap();
        std::fs::write(repo.path().join("later.txt"), "later").unwrap();
        let current = jj_snapshot(repo.path()).unwrap();
        assert_ne!(saved.commit, current.commit);
        assert_eq!(
            crate::native_jj::resolve(repo.path(), &saved.commit),
            Some(saved.commit)
        );
        assert_eq!(crate::native_jj::resolve(repo.path(), "not an id"), None);
    }

    #[test]
    fn parse_left_right_counts_splits_ahead_behind() {
        assert_eq!(parse_left_right_counts("3\t5\n"), Some((3, 5)));
        assert_eq!(parse_left_right_counts("0 0"), Some((0, 0)));
        assert_eq!(parse_left_right_counts(""), None);
    }

    #[test]
    fn git_only_build_snapshots_colocated_repository_and_nested_paths() {
        if cfg!(feature = "jj-lib") || !cfg!(feature = "git-cli") {
            return;
        }
        let root = git_repo();
        std::fs::create_dir(root.path().join(".jj")).unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert_eq!(detect_parent_vcs(&nested).unwrap(), SessionVcs::Git);
        assert!(snapshot(root.path(), "colocated-git").is_ok());
    }

    #[test]
    fn detect_vcs_prefers_jj_when_both_exist() {
        let root = tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::create_dir(root.path().join(".jj")).unwrap();
        if cfg!(feature = "jj-lib") {
            assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Jj);
        } else if cfg!(feature = "git-cli") {
            assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Git);
        } else {
            assert!(detect_vcs(root.path()).is_err());
        }
    }

    #[test]
    fn detect_vcs_git_only() {
        let root = tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        if cfg!(feature = "git-cli") {
            assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Git);
        } else {
            assert_eq!(
                detect_vcs(root.path()).unwrap_err().kind,
                VcsErrorKind::MintUnavailable
            );
        }
    }

    #[test]
    fn feature_selection_refuses_missing_backends_without_fallback() {
        let root = tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        if !cfg!(feature = "git-cli") {
            let error = snapshot(root.path(), "disabled-git").unwrap_err();
            assert_eq!(error.kind, VcsErrorKind::MintUnavailable);
            assert!(error.detail.contains("Git CLI backend is not included"));
        }
        std::fs::create_dir(root.path().join(".jj")).unwrap();
        if cfg!(feature = "jj-lib") {
            assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Jj);
        } else if cfg!(feature = "git-cli") {
            assert_eq!(detect_vcs(root.path()).unwrap(), SessionVcs::Git);
        } else {
            assert!(detect_vcs(root.path()).is_err());
        }
        std::fs::remove_dir(root.path().join(".git")).unwrap();
        if !cfg!(feature = "jj-lib") {
            let error = snapshot(root.path(), "disabled-jj").unwrap_err();
            assert_eq!(error.kind, VcsErrorKind::MintUnavailable);
            assert!(error.detail.contains("this build has no jj support"));
        }
    }

    #[test]
    fn detect_vcs_neither_is_unavailable() {
        let root = tempdir().unwrap();
        assert_eq!(
            detect_vcs(root.path()).unwrap_err().kind,
            VcsErrorKind::MintUnavailable
        );
    }

    #[test]
    fn failed_attempt_snapshot_ref_is_create_only() {
        assert_failed_snapshot_ref_is_create_only(&git_repo());
    }

    #[test]
    fn failed_attempt_snapshot_ref_is_create_only_in_sha256_repositories() {
        assert_failed_snapshot_ref_is_create_only(&git_repo_with_init(&[
            "init",
            "-b",
            "main",
            "--object-format=sha256",
        ]));
    }

    fn assert_failed_snapshot_ref_is_create_only(repo: &tempfile::TempDir) {
        let namespace = digest_text("create-only-failure");
        let id = "failed-worker-a1";
        std::fs::write(repo.path().join("SEED.txt"), "first failure\n").unwrap();
        let first = SystemVcs.seal(repo.path(), &namespace, id).unwrap();
        let repeated = SystemVcs.seal(repo.path(), &namespace, id).unwrap_err();
        let reference = candidate_ref(&namespace, id);
        assert!(repeated.detail.contains(&reference), "{repeated:?}");
        std::fs::write(repo.path().join("SEED.txt"), "later work\n").unwrap();
        let error = SystemVcs.seal(repo.path(), &namespace, id).unwrap_err();
        assert!(error.detail.contains(&reference), "{error:?}");
        assert_eq!(
            git_text(repo.path(), &["rev-parse", &reference], "ref").unwrap(),
            first.commit
        );
        assert_eq!(
            git_text(
                repo.path(),
                &["show", &format!("{reference}:SEED.txt")],
                "seed"
            )
            .unwrap(),
            "first failure"
        );
    }

    #[cfg(feature = "jj-lib")]
    #[test]
    fn failed_attempt_jj_snapshot_ref_is_create_only() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo_with_init_args(&["git", "init", "--no-colocate"]);
        let namespace = digest_text("create-only-jj-failure");
        let id = "failed-worker-a1";
        std::fs::write(repo.path().join("SEED.txt"), "first failure\n").unwrap();
        let first = SystemVcs.seal(repo.path(), &namespace, id).unwrap();
        let repeated = SystemVcs.seal(repo.path(), &namespace, id).unwrap_err();
        let reference = candidate_ref(&namespace, id);
        assert!(repeated.detail.contains(&reference), "{repeated:?}");
        std::fs::write(repo.path().join("SEED.txt"), "later work\n").unwrap();
        let error = SystemVcs.seal(repo.path(), &namespace, id).unwrap_err();
        assert!(error.detail.contains(&reference), "{error:?}");
        let output = Command::new("git")
            .arg("--git-dir=.jj/repo/store/git")
            .args(["rev-parse", &reference])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            first.commit
        );
    }

    /// the snapshot sees modified, untracked, and deleted paths,
    /// and leaves the real index and the working copy alone.
    #[test]
    fn failed_attempt_snapshot_pins_dirty_files_without_moving_branch() {
        let repo = git_repo();
        let head = git_text(repo.path(), &["rev-parse", "HEAD"], "head").unwrap();
        let branch = git_text(repo.path(), &["symbolic-ref", "HEAD"], "branch").unwrap();
        std::fs::write(repo.path().join("SEED.txt"), "partial work\n").unwrap();
        std::fs::write(repo.path().join("NEW.txt"), "untracked work\n").unwrap();
        std::fs::write(repo.path().join(".env"), "SANITIZED_FIXTURE=1\n").unwrap();
        let index = std::fs::read(repo.path().join(".git/index")).unwrap();
        let namespace = digest_text("failure-test");
        let snap = SystemVcs
            .seal(repo.path(), &namespace, "failed-worker-a1")
            .unwrap();
        let reference = candidate_ref(&namespace, "failed-worker-a1");
        let files = git_text(
            repo.path(),
            &["ls-tree", "-r", "--name-only", &reference],
            "files",
        )
        .unwrap();
        assert!(!files.contains(".env"));
        assert_eq!(
            git_text(repo.path(), &["rev-parse", &reference], "ref").unwrap(),
            snap.commit
        );
        assert_eq!(
            git_text(
                repo.path(),
                &["show", &format!("{reference}:NEW.txt")],
                "new"
            )
            .unwrap(),
            "untracked work"
        );
        assert_eq!(
            git_text(
                repo.path(),
                &["show", &format!("{reference}:SEED.txt")],
                "seed"
            )
            .unwrap(),
            "partial work"
        );
        assert_eq!(
            git_text(repo.path(), &["rev-parse", "HEAD"], "head").unwrap(),
            head
        );
        assert_eq!(
            git_text(repo.path(), &["symbolic-ref", "HEAD"], "branch").unwrap(),
            branch
        );
        assert_eq!(
            std::fs::read(repo.path().join(".git/index")).unwrap(),
            index
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("NEW.txt")).unwrap(),
            "untracked work\n"
        );
    }

    #[test]
    fn snapshot_captures_dirty_state_without_touching_the_index() {
        let repo = git_repo();
        std::fs::write(repo.path().join("SEED.txt"), "changed\n").unwrap();
        std::fs::write(repo.path().join("NEW.txt"), "new\n").unwrap();
        let before = std::fs::read(repo.path().join(".git/index")).unwrap();
        let snap = snapshot(repo.path(), "t1").unwrap();
        assert_eq!(snap.commit.len(), 40, "{}", snap.commit);
        assert_eq!(snap.content.len(), 40, "{}", snap.content);
        assert!(snap.change_id.is_none());
        let listed = git_text(
            repo.path(),
            &["ls-tree", "--name-only", snap.content.as_str()],
            "ls-tree",
        )
        .unwrap();
        assert!(listed.contains("NEW.txt"), "{listed}");
        assert_eq!(
            std::fs::read(repo.path().join(".git/index")).unwrap(),
            before,
            "the real index was rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
            "changed\n"
        );
    }

    /// The snapshot commit must not pin a secret the tool policy excludes.
    #[test]
    fn snapshot_excludes_secret_paths() {
        let repo = git_repo();
        std::fs::write(repo.path().join(".env"), "TOKEN=1\n").unwrap();
        std::fs::write(repo.path().join("app.pem"), "key\n").unwrap();
        std::fs::write(repo.path().join("keep.txt"), "keep\n").unwrap();
        let snap = snapshot(repo.path(), "secrets").unwrap();
        let listed = git_text(
            repo.path(),
            &["ls-tree", "-r", "--name-only", snap.content.as_str()],
            "ls-tree",
        )
        .unwrap();
        assert!(!listed.contains(".env"), "{listed}");
        assert!(!listed.contains("app.pem"), "{listed}");
        assert!(listed.contains("keep.txt"), "{listed}");
    }

    #[test]
    fn parse_merge_tree_conflicts_reads_the_info_section() {
        let stdout =
            "abc123\n100644 aaa 1\tsrc/a.rs\n100644 bbb 2\tsrc/a.rs\n\nCONFLICT (content)\n";
        assert_eq!(
            parse_merge_tree_conflicts(stdout),
            vec!["src/a.rs".to_owned()]
        );
    }

    #[test]
    fn snapshot_token_matches_commit_content_or_pair() {
        let snap = VcsSnapshot {
            history_left_base: false,
            commit: "c".into(),
            content: "t".into(),
            change_id: None,
        };
        assert_eq!(snap.token(), "c:t");
        assert!(snap.matches_expectation("c:t"));
        assert!(snap.matches_expectation("c"));
        assert!(snap.matches_expectation("t"));
        assert!(!snap.matches_expectation("other"));
        assert!(!snap.matches_expectation(""));
    }

    /// B4: a subprocess refusal quotes one bounded line of stderr.
    #[test]
    fn stderr_excerpt_is_one_bounded_line() {
        assert_eq!(stderr_excerpt(b""), "(no stderr)");
        assert_eq!(
            stderr_excerpt(b"error: patch failed: a.rs:1\n\nerror: a.rs: patch does not apply\n"),
            "error: patch failed: a.rs:1 | error: a.rs: patch does not apply"
        );
        let long = vec![b'x'; STDERR_EXCERPT_BYTES * 2];
        assert_eq!(stderr_excerpt(&long).len(), STDERR_EXCERPT_BYTES);
    }

    /// the hook runs in the copy with the template env and
    /// reports a non-zero exit with its output.
    #[test]
    fn run_mint_hook_reports_exit_and_output() {
        let cwd = tempdir().unwrap();
        let mut env = BTreeMap::new();
        env.insert("PLAK_HOOK_VALUE".to_owned(), "hooked".to_owned());
        run_mint_hook(cwd.path(), "printf %s \"$PLAK_HOOK_VALUE\" > out.txt", &env).unwrap();
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("out.txt")).unwrap(),
            "hooked"
        );
        let error = run_mint_hook(cwd.path(), "echo why >&2; exit 7", &env).unwrap_err();
        assert!(error.starts_with("hook exited 7:"), "{error}");
        assert!(error.contains("why"), "{error}");
    }

    /// A `git` that stalls is killed at its ceiling and reported as a
    /// timeout, and a git that answers keeps its full output; neither may
    /// prompt for credentials.
    #[test]
    fn a_stalled_git_is_killed_at_its_ceiling() {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = tempdir().unwrap();
        let fake = bin.path().join("git");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor last; do :; done\n[ \"$last\" = slow ] && /bin/sleep 30\necho \"prompt=$GIT_TERMINAL_PROMPT askpass=[$GIT_ASKPASS] $*\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake_git = |arg: &str| {
            let mut command = git_command(bin.path());
            command.arg(arg).env("PATH", bin.path());
            command
        };
        let started = std::time::Instant::now();
        let error =
            output_within(fake_git("slow"), std::time::Duration::from_millis(300)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let output = output_within(fake_git("fast"), GIT_QUERY_TIMEOUT).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "prompt=0 askpass=[] -c core.fsmonitor=false -c core.hooksPath=/dev/null fast"
        );
    }

    /// Plak's git steps inherit no `GIT_*` steering (`GIT_DIR`,
    /// `GIT_WORK_TREE`, `GIT_CONFIG_*`, ...) from the process that started it.
    #[test]
    fn git_steps_drop_inherited_git_environment() {
        let mut command = Command::new("/usr/bin/env");
        command
            .env("GIT_DIR", "/elsewhere")
            .env("GIT_WORK_TREE", "/elsewhere")
            .env("GIT_CONFIG_PARAMETERS", "'core.hookspath'='/tmp'")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_EXTERNAL_DIFF", "/tmp/differ")
            .env("PLAK_KEEP", "kept");
        strip_git_environment(&mut command);
        let output = command.output().unwrap();
        let environment = String::from_utf8_lossy(&output.stdout);
        assert!(environment.lines().any(|line| line == "PLAK_KEEP=kept"));
        assert!(!environment.lines().any(|line| line.starts_with("GIT_")));

        let built = git_command(Path::new("/"));
        let envs: Vec<_> = built.get_envs().collect();
        for name in [
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_CONFIG_COUNT",
            "GIT_SSH_COMMAND",
        ] {
            assert!(
                envs.contains(&(std::ffi::OsStr::new(name), None)),
                "{name} reaches Plak's git"
            );
        }
    }

    fn git_in(repo: &Path, args: &[&str]) -> Output {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// A script that leaves `marker` behind when anything runs it.
    fn tripwire(dir: &Path, name: &str, marker: &Path) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let script = dir.join(name);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho ran >> '{}'\necho garbage\nexit 1\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.display().to_string()
    }

    /// Repository hooks and the fsmonitor hook never run inside a mint, seal
    /// or adopt, even when the repository points `core.hooksPath` at failing
    /// hooks.
    #[test]
    fn repository_hooks_never_run_in_plak_git_steps() {
        let repo = git_repo();
        let tools = tempdir().unwrap();
        let marker = tools.path().join("ran");
        let hooks = tools.path().join("hooks");
        std::fs::create_dir(&hooks).unwrap();
        for hook in [
            "post-checkout",
            "pre-commit",
            "post-index-change",
            "reference-transaction",
        ] {
            tripwire(&hooks, hook, &marker);
        }
        let monitor = tripwire(tools.path(), "fsmonitor", &marker);
        git_in(
            repo.path(),
            &["config", "core.hooksPath", &hooks.display().to_string()],
        );
        git_in(repo.path(), &["config", "core.fsmonitor", &monitor]);

        let minted = SystemVcs
            .mint_from(
                repo.path(),
                "c1",
                "c1",
                repo.path(),
                WorkspaceMode::Fork,
                None,
            )
            .unwrap();
        std::fs::write(minted.path.join("SEED.txt"), "child\n").unwrap();
        let candidate = SystemVcs
            .seal(&minted.path, &digest_text("fixture"), "cand-1")
            .unwrap();
        SystemVcs
            .adopt(
                repo.path(),
                &minted.path,
                &minted.base.commit,
                None,
                &candidate,
                None,
            )
            .unwrap();
        SystemVcs.unmint(repo.path(), &minted.path).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
            "child\n"
        );
        assert!(!marker.exists(), "a repository hook ran inside Plak's git");
    }

    /// External diff drivers and textconv filters configured by the
    /// repository are ignored by the adopt diff, so the landed bytes are the
    /// candidate's own and no configured program runs.
    #[test]
    fn adopt_ignores_external_diff_and_textconv() {
        let repo = git_repo();
        let tools = tempdir().unwrap();
        let marker = tools.path().join("ran");
        let external = tripwire(tools.path(), "external", &marker);
        let textconv = tripwire(tools.path(), "textconv", &marker);
        git_in(repo.path(), &["config", "diff.external", &external]);
        git_in(repo.path(), &["config", "diff.evil.textconv", &textconv]);
        std::fs::write(repo.path().join(".gitattributes"), "*.txt diff=evil\n").unwrap();
        git_in(repo.path(), &["add", ".gitattributes"]);
        git_in(repo.path(), &["commit", "-q", "-m", "attributes"]);

        let minted = SystemVcs
            .mint_from(
                repo.path(),
                "c1",
                "c1",
                repo.path(),
                WorkspaceMode::Fork,
                None,
            )
            .unwrap();
        std::fs::write(minted.path.join("SEED.txt"), "seed\nchild line\n").unwrap();
        std::fs::write(minted.path.join("NEW.txt"), "new\n").unwrap();
        let candidate = SystemVcs
            .seal(&minted.path, &digest_text("fixture"), "cand-1")
            .unwrap();
        let landing = SystemVcs
            .adopt(
                repo.path(),
                &minted.path,
                &minted.base.commit,
                None,
                &candidate,
                None,
            )
            .unwrap();
        assert_eq!(landing.files_changed, 2);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
            "seed\nchild line\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("NEW.txt")).unwrap(),
            "new\n"
        );
        assert!(
            !marker.exists(),
            "a configured diff program ran inside the adopt"
        );
    }

    /// A copy whose directory and registration are already gone (an earlier
    /// removal, or by hand) unmints cleanly: only its branch is left to drop,
    /// and a repeat is a no-op. A directory that is still present and
    /// registered is removed as before.
    #[test]
    fn unmint_of_an_already_removed_copy_drops_only_its_branch() {
        let repo = git_repo();
        let copy = SystemVcs
            .mint_from(
                repo.path(),
                "c1",
                "t-c1",
                repo.path(),
                WorkspaceMode::Fork,
                None,
            )
            .unwrap();
        let removed = git_in(
            repo.path(),
            &[
                "worktree",
                "remove",
                "--force",
                copy.path.display().to_string().as_str(),
            ],
        );
        assert!(removed.status.success());
        assert!(!copy.path.exists());
        let branch = |repo: &Path| {
            git_run(
                repo,
                &["show-ref", "--verify", "--quiet", "refs/heads/plak/t-c1"],
            )
            .unwrap()
            .status
            .success()
        };
        assert!(branch(repo.path()), "the branch outlives a by-hand removal");

        SystemVcs.unmint(repo.path(), &copy.path).unwrap();
        assert!(!branch(repo.path()), "unmint dropped the leftover branch");
        SystemVcs.unmint(repo.path(), &copy.path).unwrap();

        let live = SystemVcs
            .mint_from(
                repo.path(),
                "c2",
                "t-c2",
                repo.path(),
                WorkspaceMode::Fork,
                None,
            )
            .unwrap();
        SystemVcs.unmint(repo.path(), &live.path).unwrap();
        assert!(!live.path.exists());
        let listed = String::from_utf8(git_in(repo.path(), &["worktree", "list"]).stdout).unwrap();
        assert!(!listed.contains("t-c2"), "{listed}");
    }

    /// A crash between `git worktree add` and the tree recording the copy
    /// leaves a registered worktree and its `plak/<copy_id>` branch; a bare
    /// directory is left by a crash before registration. The next mint of
    /// that copy id reclaims either instead of refusing.
    #[test]
    fn the_next_mint_reclaims_an_orphaned_copy() {
        let repo = git_repo();
        let orphan = SystemVcs
            .mint_from(
                repo.path(),
                "c1",
                "t-c1",
                repo.path(),
                WorkspaceMode::Fork,
                None,
            )
            .unwrap();
        std::fs::write(orphan.path.join("STALE.txt"), "never recorded\n").unwrap();
        let reminted = SystemVcs
            .mint_from(
                repo.path(),
                "c1",
                "t-c1",
                repo.path(),
                WorkspaceMode::Snapshot,
                None,
            )
            .unwrap();
        assert_eq!(reminted.path, orphan.path);
        assert!(!reminted.path.join("STALE.txt").exists());
        assert!(
            !git_run(
                repo.path(),
                &["show-ref", "--verify", "--quiet", "refs/heads/plak/t-c1"]
            )
            .unwrap()
            .status
            .success(),
            "the orphan's branch survived"
        );
        let listed = String::from_utf8(git_in(repo.path(), &["worktree", "list"]).stdout).unwrap();
        assert_eq!(listed.matches("t-c1").count(), 1, "{listed}");

        // A directory with no registration at all.
        SystemVcs.unmint(repo.path(), &reminted.path).unwrap();
        std::fs::create_dir_all(reminted.path.join("partial")).unwrap();
        let fresh = SystemVcs
            .mint_from(
                repo.path(),
                "c1",
                "t-c1",
                repo.path(),
                WorkspaceMode::Fork,
                None,
            )
            .unwrap();
        assert!(!fresh.path.join("partial").exists());
        assert!(fresh.path.join("SEED.txt").is_file());
    }

    /// Patches, snapshot indexes and captured git output never land in a
    /// shared, guessable temp path: each step gets its own 0700 directory and
    /// 0600 files, and they are gone when the step returns.
    #[test]
    fn scratch_files_are_private_and_removed() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = private_scratch().unwrap();
        let dir_mode = std::fs::metadata(scratch.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        let file = scratch.path().join("patch");
        drop(create_private(&file).unwrap());
        let file_mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
        assert!(
            create_private(&file).is_err(),
            "an existing file is never reused"
        );
        let kept = scratch.path().to_path_buf();
        drop(scratch);
        assert!(!kept.exists());
    }

    /// the hook is a shell, and a shell's children outlive
    /// `kill(sh)`. A timed-out `on_mint` that had started `cargo build` left
    /// it running on the copy's target lock, so the timeout kills the whole
    /// process group.
    #[test]
    fn a_timed_out_mint_hook_kills_the_whole_process_group() {
        let cwd = tempdir().unwrap();
        let env = BTreeMap::new();
        let error = run_mint_hook_bounded(
            cwd.path(),
            "sh -c 'sleep 100 & echo $! > child.pid; wait'",
            &env,
            std::time::Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(error.contains("was killed"), "{error}");
        let pid = std::fs::read_to_string(cwd.path().join("child.pid"))
            .unwrap()
            .trim()
            .to_owned();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let alive = Command::new("sh")
                .arg("-c")
                .arg(format!("kill -0 {pid}"))
                .status()
                .unwrap()
                .success();
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the hook's grandchild {pid} outlived the timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// the hook starts from a cleared environment — PATH, HOME
    /// and the supplied hook map, nothing else — so an exported
    /// `*_API_KEY` never reaches a template-authored shell command.
    #[test]
    fn a_mint_hook_does_not_inherit_the_hosts_environment() {
        let cwd = tempdir().unwrap();
        let mut env = BTreeMap::new();
        env.insert("PLAK_HOOK_VALUE".to_owned(), "hooked".to_owned());
        env.insert("PLAK_LAUNCH_CHECKOUT".to_owned(), "/launch".to_owned());
        env.insert("PLAK_PARENT_PATH".to_owned(), "/parent-copy".to_owned());
        env.insert("PLAK_COPY_ID".to_owned(), "tree-c2".to_owned());
        run_mint_hook(cwd.path(), "env > out.txt", &env).unwrap();
        let seen = std::fs::read_to_string(cwd.path().join("out.txt")).unwrap();
        let names: Vec<&str> = seen
            .lines()
            .filter_map(|line| line.split('=').next())
            .filter(|name| !name.is_empty())
            .collect();
        for name in &names {
            assert!(
                // `sh` sets the last three itself.
                matches!(
                    *name,
                    "PATH"
                        | "HOME"
                        | "PLAK_HOOK_VALUE"
                        | "PLAK_LAUNCH_CHECKOUT"
                        | "PLAK_PARENT_PATH"
                        | "PLAK_COPY_ID"
                        | "PWD"
                        | "SHLVL"
                        | "_"
                ),
                "the hook inherited {name} from the host: {seen}"
            );
        }
        assert!(names.contains(&"PATH"), "{seen}");
        assert!(names.contains(&"PLAK_HOOK_VALUE"), "{seen}");
        for (name, value) in &env {
            assert!(
                seen.lines().any(|line| line == format!("{name}={value}")),
                "{seen}"
            );
        }
        assert!(
            std::env::vars().count() > names.len(),
            "the host env is wider than what the hook saw"
        );
    }
}
