use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::{
    AdoptFailure, AdoptLanding, AdoptRequest, MintRequest, MintedCopy, VcsError, VcsFuture, VcsOps,
    VcsSnapshot,
};
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
    /// [`VcsOps::restore_copy_base`].
    RestoreBase,
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
    /// [`VcsOps::prepare_refresh`].
    PrepareRefresh,
    /// [`VcsOps::apply_refresh`].
    ApplyRefresh,
    /// [`VcsOps::path_exists`].
    Exists,
    /// [`VcsOps::jj_copy_dirs`].
    CopyDirs,
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

#[derive(Clone, Copy, PartialEq)]
enum Placement {
    Ready,
    Held,
    Refused,
}

/// A [`VcsOps`] that records every call instead of running it: copies are
/// plain directories under `<launch>/.plak/fake/<agent_id>`.
#[derive(Default)]
pub struct FakeVcs {
    placements: Mutex<BTreeMap<PathBuf, Placement>>,
    calls: Mutex<Vec<FakeCall>>,
    lock_probe: Mutex<Option<LockProbe>>,
    next_seal: Mutex<Option<Result<VcsSnapshot, VcsError>>>,
    next_adopt: Mutex<Option<Result<AdoptLanding, AdoptFailure>>>,
    next_hook: Mutex<Option<Result<(), VcsError>>>,
    next_unmint: Mutex<Option<Result<(), VcsError>>>,
    next_unpin: Mutex<Option<Result<(), VcsError>>>,
    next_restore_base: Mutex<Option<Result<(), VcsError>>>,
    label: Mutex<Option<String>>,
    scripted_refresh: std::sync::atomic::AtomicBool,
    unreachable: std::sync::atomic::AtomicBool,
}

impl FakeVcs {
    /// Sets a managed placement's barrier for deterministic admission cuts.
    pub fn set_placement_ready(&self, path: PathBuf, ready: bool) {
        self.placements
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                path,
                if ready {
                    Placement::Ready
                } else {
                    Placement::Held
                },
            );
    }
    /// Refuses a placement rather than leaving it in a releasable hold.
    pub fn refuse_placement(&self, path: PathBuf) {
        self.placements
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path, Placement::Refused);
    }
    /// Enables a deterministic Ready/Refreshed pair for indexed refresh cuts.
    pub fn script_refresh(&self) {
        self.scripted_refresh
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    /// From now on every question about the workspace's files (whether a
    /// path exists, the JJ copy root, a copy's lineage) answers `Err`, as a
    /// port that cannot reach the workspace does.
    pub fn refuse_workspace_questions(&self) {
        self.unreachable
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn unreachable(&self) -> Result<(), VcsError> {
        if self.unreachable.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(VcsError::mint_unavailable(
                "the fake workspace is unreachable",
            ));
        }
        Ok(())
    }

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

    /// Injects one copy-base reconciliation result.
    pub fn restore_base_next_with(&self, answer: Result<(), VcsError>) {
        *self
            .next_restore_base
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

#[allow(
    clippy::unnecessary_wraps,
    reason = "each answer keeps the shape of the port method it answers"
)]
impl FakeVcs {
    fn answer_prepare_refresh(
        &self,
        request: &super::RefreshRequest,
    ) -> Result<super::RefreshPreparation, VcsError> {
        self.record(FakeOp::PrepareRefresh, &request.child);
        if self
            .scripted_refresh
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(super::RefreshPreparation::Ready(Box::new(
                super::RefreshPlan {
                    request: request.clone(),
                    parent: "refresh-parent".into(),
                    parent_content: "refresh-parent-tree".into(),
                    child: "refresh-child".into(),
                    child_content: "refresh-child-tree".into(),
                    merged_content: "refresh-merged-tree".into(),
                    head_old: "refresh-old-head".into(),
                    merge_commit: "refresh-merge".into(),
                },
            )));
        }
        Ok(super::RefreshPreparation::Unchanged)
    }

    fn answer_apply_refresh(
        &self,
        plan: &super::RefreshPlan,
        _recover: bool,
    ) -> Result<super::RefreshApplied, VcsError> {
        self.record(FakeOp::ApplyRefresh, &plan.request.child);
        if self
            .scripted_refresh
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            Ok(super::RefreshApplied::Refreshed)
        } else {
            Err(VcsError::mint_unavailable("fake refresh was not scripted"))
        }
    }
    fn answer_restore_copy_base(
        &self,
        path: &Path,
        _namespace: &str,
        _agent: &str,
        _base: &str,
    ) -> Result<(), VcsError> {
        self.record(FakeOp::RestoreBase, path);
        self.next_restore_base
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or(Ok(()))
    }

    fn answer_mint_from(&self, request: &MintRequest<'_>) -> Result<MintedCopy, VcsError> {
        let MintRequest {
            launch,
            agent_id,
            mode,
            base,
            namespace,
            ..
        } = *request;
        let path = Self::copy_dir(launch, agent_id);
        let tree8 = namespace
            .get(..8)
            .ok_or_else(|| VcsError::mint_unavailable("invalid tree namespace"))?;
        self.record(FakeOp::Mint, &path);
        std::fs::create_dir_all(&path)
            .map_err(|error| VcsError::mint_failed(format!("fake mint: {error}")))?;
        Ok(MintedCopy {
            path,
            branch: (mode == WorkspaceMode::Fork)
                .then(|| super::copy_names(tree8, agent_id).branch),
            base: base.cloned().unwrap_or_else(|| Self::base_for(agent_id)),
        })
    }

    fn answer_seal(
        &self,
        child_path: &Path,
        _namespace: &str,
        candidate_id: &str,
        _base_commit: &str,
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

    fn answer_unpin(
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

    fn answer_adopt(&self, request: &AdoptRequest<'_>) -> Result<AdoptLanding, AdoptFailure> {
        let AdoptRequest {
            target_path,
            candidate,
            ..
        } = *request;
        self.record(FakeOp::Adopt, target_path);
        if let Some(reason) = request.resolution_refusal {
            return Err(AdoptFailure::NotApplicable(reason.to_owned()));
        }
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
            files: Vec::new(),
            child_base: None,
        })
    }

    fn answer_run_mint_hook(
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

    fn answer_unmint(&self, _launch: &Path, path: &Path, _namespace: &str) -> Result<(), VcsError> {
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

    fn answer_dirty(&self, path: &Path) -> bool {
        self.record(FakeOp::Dirty, path);
        false
    }

    fn answer_ahead_behind(
        &self,
        child: &Path,
        _parent: &Path,
        _base: Option<&str>,
    ) -> Option<(u64, u64)> {
        self.record(FakeOp::AheadBehind, child);
        None
    }

    fn answer_label(&self, path: &Path) -> Option<String> {
        self.record(FakeOp::Label, path);
        self.label
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn answer_diverged_from_base(
        &self,
        path: &Path,
        _base_change_id: Option<&str>,
        _base_commit: &str,
    ) -> Result<bool, VcsError> {
        self.record(FakeOp::Diverged, path);
        Ok(true)
    }

    fn answer_copy_is_over_candidate(
        &self,
        launch: &Path,
        _agent_id: &str,
        _candidate: &VcsSnapshot,
    ) -> Result<bool, VcsError> {
        self.record(FakeOp::OverCandidate, launch);
        self.unreachable()?;
        Ok(false)
    }

    fn answer_path_exists(&self, path: &Path) -> Result<bool, VcsError> {
        self.record(FakeOp::Exists, path);
        self.unreachable()?;
        Ok(path.exists())
    }

    fn answer_jj_copy_dirs(&self, launch: &Path) -> Result<BTreeSet<String>, VcsError> {
        self.record(FakeOp::CopyDirs, launch);
        self.unreachable()?;
        super::SystemVcs.jj_copy_dirs(launch)
    }
}

/// Each answer is computed when the call is made, so a recorded call's lock
/// probe sees the caller's lock state at the call.
impl VcsOps for FakeVcs {
    fn placement_ready(&self, path: &Path) -> Option<bool> {
        self.placements
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, placement)| *placement == Placement::Ready)
    }

    fn placement_held(&self, path: &Path) -> bool {
        self.placements
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .is_some_and(|(_, placement)| *placement == Placement::Held)
    }

    fn prepare_refresh<'a>(
        &'a self,
        request: &'a super::RefreshRequest,
    ) -> VcsFuture<'a, Result<super::RefreshPreparation, VcsError>> {
        ready(self.answer_prepare_refresh(request))
    }

    fn apply_refresh<'a>(
        &'a self,
        plan: &'a super::RefreshPlan,
        recover: bool,
    ) -> VcsFuture<'a, Result<super::RefreshApplied, VcsError>> {
        ready(self.answer_apply_refresh(plan, recover))
    }

    fn restore_copy_base<'a>(
        &'a self,
        path: &'a Path,
        namespace: &'a str,
        agent: &'a str,
        base: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        ready(self.answer_restore_copy_base(path, namespace, agent, base))
    }

    fn path_exists<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Result<bool, VcsError>> {
        ready(self.answer_path_exists(path))
    }

    fn jj_copy_dirs<'a>(
        &'a self,
        launch: &'a Path,
    ) -> VcsFuture<'a, Result<BTreeSet<String>, VcsError>> {
        ready(self.answer_jj_copy_dirs(launch))
    }

    fn mint_from<'a>(
        &'a self,
        request: &'a MintRequest<'_>,
    ) -> VcsFuture<'a, Result<MintedCopy, VcsError>> {
        ready(self.answer_mint_from(request))
    }

    fn seal<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        ready(self.answer_seal(child_path, namespace, candidate_id, base_commit))
    }

    fn unpin<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        ready(self.answer_unpin(child_path, namespace, candidate_id))
    }

    fn adopt<'a>(
        &'a self,
        request: &'a AdoptRequest<'_>,
    ) -> VcsFuture<'a, Result<AdoptLanding, AdoptFailure>> {
        ready(self.answer_adopt(request))
    }

    fn run_mint_hook<'a>(
        &'a self,
        cwd: &'a Path,
        command: &'a str,
        env: &'a BTreeMap<String, String>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        ready(self.answer_run_mint_hook(cwd, command, env))
    }

    fn unmint<'a>(
        &'a self,
        launch: &'a Path,
        path: &'a Path,
        namespace: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        ready(self.answer_unmint(launch, path, namespace))
    }

    fn dirty<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, bool> {
        ready(self.answer_dirty(path))
    }

    fn ahead_behind<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        ready(self.answer_ahead_behind(child, parent, base))
    }

    fn label<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Option<String>> {
        ready(self.answer_label(path))
    }

    fn diverged_from_base<'a>(
        &'a self,
        path: &'a Path,
        base_change_id: Option<&'a str>,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        ready(self.answer_diverged_from_base(path, base_change_id, base_commit))
    }

    fn copy_is_over_candidate<'a>(
        &'a self,
        launch: &'a Path,
        copy_id: &'a str,
        candidate: &'a VcsSnapshot,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        ready(self.answer_copy_is_over_candidate(launch, copy_id, candidate))
    }
}

fn ready<'a, T: Send + 'a>(value: T) -> VcsFuture<'a, T> {
    Box::pin(std::future::ready(value))
}
