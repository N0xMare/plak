//! Version control on a runner (spec 01 §8.4, spec 03 §14): the tree's
//! [`VcsOps`] port carried over the runner link, and the runner side that
//! answers it with the system port over the runner's own filesystem.
//!
//! Questions go as unkeyed reads (`vcs.read`). Effects (a mint, seal,
//! adopt, unmint, hook, refresh or pin) go as `vcs.effect` steps of a tree
//! operation, keyed by the tree and the stage the tree recorded for the
//! effect before it ran ([`stage`]), so the runner runs each step at most
//! once and a host that loses the link replays it by key. An effect run
//! in an unstaged scope gets a fresh key of its own. A step whose outcome the
//! link could not learn marks its stage unknown and answers a
//! recovery-required error; one the link refused answers unavailable, with
//! no effect.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous::tools::runner::{
    RunnerCall, RunnerExtension, RunnerFailure, RunnerLink, RunnerVerb, ServeFailure, ServeFuture,
};
use plakous_workspace::occupancy_vcs::{
    AdoptFailure, AdoptLanding, AdoptRequest, BundleFacts, MintRequest, MintedCopy, OwnedAdopt,
    OwnedMint, RefreshApplied, RefreshPlan, RefreshPreparation, RefreshRequest, SystemVcs,
    VcsError, VcsErrorKind, VcsFuture, VcsOps, VcsSnapshot, stage,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One question or effect, as the link carries it.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum VcsCall {
    PathExists {
        path: PathBuf,
    },
    JjCopyDirs {
        launch: PathBuf,
    },
    Label {
        path: PathBuf,
    },
    Dirty {
        path: PathBuf,
    },
    AheadBehind {
        child: PathBuf,
        parent: PathBuf,
        base: Option<String>,
    },
    DivergedFromBase {
        path: PathBuf,
        base_change_id: Option<String>,
        base_commit: String,
    },
    CopyIsOverCandidate {
        launch: PathBuf,
        copy_id: String,
        candidate: VcsSnapshot,
    },
    HistoryLeftBase {
        path: PathBuf,
        base: String,
        sealed: String,
    },
    CheckReclaimable {
        path: PathBuf,
        sealed: Option<VcsSnapshot>,
    },
    PrepareRefresh {
        request: RefreshRequest,
    },
    Mint {
        request: OwnedMint,
    },
    Seal {
        child_path: PathBuf,
        namespace: String,
        candidate_id: String,
        base_commit: String,
    },
    Unpin {
        child_path: PathBuf,
        namespace: String,
        candidate_id: String,
    },
    RestoreCopyBase {
        path: PathBuf,
        namespace: String,
        agent: String,
        base: String,
    },
    Adopt {
        request: Box<OwnedAdopt>,
    },
    MintHook {
        cwd: PathBuf,
        command: String,
        env: BTreeMap<String, String>,
    },
    Unmint {
        launch: PathBuf,
        path: PathBuf,
        namespace: String,
    },
    ApplyRefresh {
        plan: RefreshPlan,
        recover: bool,
    },
    TrimDisposableTarget {
        path: PathBuf,
    },
    TransferRoot {
        runner_id: String,
        tree_id: String,
    },
    SweepTransfers {
        repo: PathBuf,
        max_age_secs: u64,
    },
    SnapshotBase {
        parent_cwd: PathBuf,
        namespace: String,
        agent: String,
    },
    BundleCreate {
        repo: PathBuf,
        reference: String,
        exclude: Vec<String>,
    },
    BundleRead {
        repo: PathBuf,
        id: String,
        offset: u64,
        len: u64,
    },
    BundleDrop {
        repo: PathBuf,
        id: String,
    },
    BundleWrite {
        repo: PathBuf,
        id: String,
        offset: u64,
        /// The chunk, lowercase hex.
        bytes: String,
    },
    BundleApply {
        repo: PathBuf,
        id: String,
        digest: String,
        namespace: String,
        fetch: Vec<(String, String)>,
        pins: Vec<(String, String)>,
    },
    RefCommit {
        repo: PathBuf,
        reference: String,
    },
}

impl VcsCall {
    /// Every workspace path the call touches, which the runner must admit.
    fn paths(&self) -> Vec<&Path> {
        match self {
            Self::PathExists { path }
            | Self::Label { path }
            | Self::Dirty { path }
            | Self::DivergedFromBase { path, .. }
            | Self::HistoryLeftBase { path, .. }
            | Self::CheckReclaimable { path, .. }
            | Self::RestoreCopyBase { path, .. }
            | Self::TrimDisposableTarget { path } => vec![path],
            Self::TransferRoot { .. } => Vec::new(),
            Self::SnapshotBase { parent_cwd, .. } => vec![parent_cwd],
            Self::BundleCreate { repo, .. }
            | Self::BundleRead { repo, .. }
            | Self::BundleDrop { repo, .. }
            | Self::BundleWrite { repo, .. }
            | Self::BundleApply { repo, .. }
            | Self::RefCommit { repo, .. }
            | Self::SweepTransfers { repo, .. } => vec![repo],
            Self::JjCopyDirs { launch } | Self::CopyIsOverCandidate { launch, .. } => {
                vec![launch]
            }
            Self::AheadBehind { child, parent, .. } => vec![child, parent],
            Self::PrepareRefresh { request } => vec![&request.parent, &request.child],
            Self::ApplyRefresh { plan, .. } => vec![&plan.request.parent, &plan.request.child],
            Self::Mint { request } => {
                let request = request.request();
                vec![request.launch, request.parent_cwd]
            }
            Self::Seal { child_path, .. } | Self::Unpin { child_path, .. } => vec![child_path],
            Self::Adopt { request } => {
                let request = request.request();
                vec![request.target_path, request.child_path]
            }
            Self::MintHook { cwd, .. } => vec![cwd],
            Self::Unmint { launch, path, .. } => vec![launch, path],
        }
    }
}

/// The tree's port over a runner link: every answer comes from the runner
/// that hosts the tree's workspaces.
pub struct RemoteVcs {
    link: Arc<dyn RunnerLink>,
    runner_id: String,
    tree_id: String,
}

impl RemoteVcs {
    /// The port of tree `tree_id`, whose workspaces live on `runner_id`
    /// behind `link`.
    #[must_use]
    pub fn new(link: Arc<dyn RunnerLink>, runner_id: String, tree_id: String) -> Self {
        Self {
            link,
            runner_id,
            tree_id,
        }
    }

    /// Asks one question; `Err` when the runner could not answer it.
    async fn read<T: DeserializeOwned>(&self, call: VcsCall) -> Result<T, VcsError> {
        let answer = self
            .link
            .call(RunnerCall::unkeyed(RunnerVerb::VcsRead, encode(&call)?))
            .await
            .map_err(|failure| link_error(&failure, None))?;
        decode(answer)?
    }

    /// Runs one effect as the next step of the current stage, or under a
    /// fresh key outside any stage.
    async fn effect<T: DeserializeOwned>(&self, call: VcsCall) -> Result<T, VcsError> {
        let stage = stage::current();
        if stage.is_none() {
            let reason = stage::unstaged_reason();
            debug_assert!(
                reason.is_some(),
                "a runner VCS effect ran as neither a stage nor an unstaged scope"
            );
            tracing::debug!(
                tree = %self.tree_id,
                reason = reason.unwrap_or("none given"),
                "unstaged runner VCS effect"
            );
        }
        let operation_id = stage.as_ref().map_or_else(
            || format!("plak:vcs:{}", uuid::Uuid::new_v4().simple()),
            |stage| stage.next_step(),
        );
        let payload = encode(&call)?;
        let send = |key: String| {
            self.link.call(RunnerCall::tree(
                RunnerVerb::VcsEffect,
                payload.clone(),
                self.tree_id.clone(),
                key,
            ))
        };
        // A stage taken again sends each step under its first key: the
        // runner answers a step a lost host's run still runs once it ends,
        // and the stored answer of one it finished. A step it ran to its end
        // whose answer was taken already finished, so it runs again, alone,
        // under a key of its own.
        let answer = match send(operation_id.clone()).await {
            Err(RunnerFailure::Spent) if stage.is_some() => {
                send(format!("{operation_id}.{}", uuid::Uuid::new_v4().simple())).await
            }
            answer => answer,
        }
        .map_err(|failure| link_error(&failure, stage.as_deref()))?;
        decode(answer)?
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

fn encode(call: &VcsCall) -> Result<Value, VcsError> {
    serde_json::to_value(call)
        .map_err(|_| VcsError::mint_unavailable("the VCS request could not be encoded"))
}

fn decode<T: DeserializeOwned>(answer: Value) -> Result<Result<T, VcsError>, VcsError> {
    serde_json::from_value(answer)
        .map_err(|_| VcsError::mint_unavailable("the runner's VCS answer was malformed"))
}

/// What a link failure means for a VCS call: no effect, or an outcome the
/// host cannot know, which marks `stage` unknown.
fn link_error(failure: &RunnerFailure, stage: Option<&stage::VcsStage>) -> VcsError {
    match failure {
        RunnerFailure::OutcomeUnknown(detail) | RunnerFailure::ReadInterrupted(detail) => {
            if let Some(stage) = stage {
                stage.mark_unknown();
            }
            VcsError::new(
                VcsErrorKind::RecoveryRequired,
                format!("the runner's VCS step may have run: {detail}"),
            )
        }
        RunnerFailure::Spent => {
            if let Some(stage) = stage {
                stage.mark_unknown();
            }
            VcsError::new(
                VcsErrorKind::RecoveryRequired,
                "the runner ran the VCS step and its answer was already taken",
            )
        }
        RunnerFailure::Unavailable(detail)
        | RunnerFailure::NotSent(detail)
        | RunnerFailure::Refused(detail) => {
            VcsError::mint_unavailable(format!("the runner did not run the VCS step: {detail}"))
        }
        RunnerFailure::Unsupported => {
            VcsError::mint_unavailable("the runner does not carry version control")
        }
        RunnerFailure::StaleOwner => {
            VcsError::mint_unavailable("the runner refused the VCS step's owner")
        }
        RunnerFailure::NotHolder => VcsError::mint_unavailable(
            "another process of this host holds the VCS step's line on the runner",
        ),
        RunnerFailure::Backend(error) => {
            VcsError::mint_unavailable(format!("the runner refused the VCS step: {error}"))
        }
        _ => VcsError::mint_unavailable("the runner did not run the VCS step"),
    }
}

/// An adopt's failure from a VCS error of the port.
fn adopt_failure(error: VcsError) -> AdoptFailure {
    if error.kind == VcsErrorKind::RecoveryRequired {
        AdoptFailure::RecoveryRequired(error.detail)
    } else {
        AdoptFailure::Unavailable(error.detail)
    }
}

impl VcsOps for RemoteVcs {
    fn stage_runner(&self) -> Option<&str> {
        Some(&self.runner_id)
    }

    fn prepare_refresh<'a>(
        &'a self,
        request: &'a RefreshRequest,
    ) -> VcsFuture<'a, Result<RefreshPreparation, VcsError>> {
        Box::pin(self.read(VcsCall::PrepareRefresh {
            request: request.clone(),
        }))
    }

    fn apply_refresh<'a>(
        &'a self,
        plan: &'a RefreshPlan,
        recover: bool,
    ) -> VcsFuture<'a, Result<RefreshApplied, VcsError>> {
        Box::pin(self.effect(VcsCall::ApplyRefresh {
            plan: plan.clone(),
            recover,
        }))
    }

    fn check_reclaimable<'a>(
        &'a self,
        path: &'a Path,
        sealed: Option<&'a VcsSnapshot>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(self.read(VcsCall::CheckReclaimable {
            path: path.to_owned(),
            sealed: sealed.cloned(),
        }))
    }

    fn trim_disposable_target<'a>(
        &'a self,
        path: &'a Path,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        Box::pin(self.effect(VcsCall::TrimDisposableTarget {
            path: path.to_owned(),
        }))
    }

    fn path_exists<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Result<bool, VcsError>> {
        Box::pin(self.read(VcsCall::PathExists {
            path: path.to_owned(),
        }))
    }

    fn jj_copy_dirs<'a>(
        &'a self,
        launch: &'a Path,
    ) -> VcsFuture<'a, Result<BTreeSet<String>, VcsError>> {
        Box::pin(self.read(VcsCall::JjCopyDirs {
            launch: launch.to_owned(),
        }))
    }

    fn mint_from<'a>(
        &'a self,
        request: &'a MintRequest<'_>,
    ) -> VcsFuture<'a, Result<MintedCopy, VcsError>> {
        Box::pin(self.effect(VcsCall::Mint {
            request: OwnedMint::of(request),
        }))
    }

    fn history_left_base<'a>(
        &'a self,
        path: &'a Path,
        base: &'a str,
        sealed: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        Box::pin(self.read(VcsCall::HistoryLeftBase {
            path: path.to_owned(),
            base: base.to_owned(),
            sealed: sealed.to_owned(),
        }))
    }

    fn seal<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        Box::pin(self.effect(VcsCall::Seal {
            child_path: child_path.to_owned(),
            namespace: namespace.to_owned(),
            candidate_id: candidate_id.to_owned(),
            base_commit: base_commit.to_owned(),
        }))
    }

    fn unpin<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(self.effect(VcsCall::Unpin {
            child_path: child_path.to_owned(),
            namespace: namespace.to_owned(),
            candidate_id: candidate_id.to_owned(),
        }))
    }

    fn restore_copy_base<'a>(
        &'a self,
        path: &'a Path,
        namespace: &'a str,
        agent: &'a str,
        base: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(self.effect(VcsCall::RestoreCopyBase {
            path: path.to_owned(),
            namespace: namespace.to_owned(),
            agent: agent.to_owned(),
            base: base.to_owned(),
        }))
    }

    fn adopt<'a>(
        &'a self,
        request: &'a AdoptRequest<'_>,
    ) -> VcsFuture<'a, Result<AdoptLanding, AdoptFailure>> {
        Box::pin(async move {
            match self
                .effect::<Result<AdoptLanding, AdoptFailure>>(VcsCall::Adopt {
                    request: Box::new(OwnedAdopt::of(request)),
                })
                .await
            {
                Ok(landed) => landed,
                Err(error) => Err(adopt_failure(error)),
            }
        })
    }

    fn run_mint_hook<'a>(
        &'a self,
        cwd: &'a Path,
        command: &'a str,
        env: &'a BTreeMap<String, String>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(self.effect(VcsCall::MintHook {
            cwd: cwd.to_owned(),
            command: command.to_owned(),
            env: env.clone(),
        }))
    }

    fn unmint<'a>(
        &'a self,
        launch: &'a Path,
        path: &'a Path,
        namespace: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(self.effect(VcsCall::Unmint {
            launch: launch.to_owned(),
            path: path.to_owned(),
            namespace: namespace.to_owned(),
        }))
    }

    fn transfer_root<'a>(&'a self, tree_id: &'a str) -> VcsFuture<'a, Result<PathBuf, VcsError>> {
        Box::pin(self.read(VcsCall::TransferRoot {
            runner_id: self.runner_id.clone(),
            tree_id: tree_id.to_owned(),
        }))
    }

    fn sweep_transfers<'a>(
        &'a self,
        repo: &'a Path,
        max_age_secs: u64,
    ) -> VcsFuture<'a, Result<u32, VcsError>> {
        Box::pin(self.effect(VcsCall::SweepTransfers {
            repo: repo.to_owned(),
            max_age_secs,
        }))
    }

    fn snapshot_base<'a>(
        &'a self,
        parent_cwd: &'a Path,
        namespace: &'a str,
        agent: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        Box::pin(self.effect(VcsCall::SnapshotBase {
            parent_cwd: parent_cwd.to_owned(),
            namespace: namespace.to_owned(),
            agent: agent.to_owned(),
        }))
    }

    fn bundle_create<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
        exclude: &'a [String],
    ) -> VcsFuture<'a, Result<BundleFacts, VcsError>> {
        Box::pin(self.effect(VcsCall::BundleCreate {
            repo: repo.to_owned(),
            reference: reference.to_owned(),
            exclude: exclude.to_vec(),
        }))
    }

    fn bundle_read<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        len: u64,
    ) -> VcsFuture<'a, Result<Vec<u8>, VcsError>> {
        Box::pin(async move {
            let hex: String = self
                .read(VcsCall::BundleRead {
                    repo: repo.to_owned(),
                    id: id.to_owned(),
                    offset,
                    len,
                })
                .await?;
            unhex(&hex).ok_or_else(|| {
                VcsError::mint_unavailable("the runner's bundle chunk was malformed")
            })
        })
    }

    fn bundle_drop<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(self.effect(VcsCall::BundleDrop {
            repo: repo.to_owned(),
            id: id.to_owned(),
        }))
    }

    fn bundle_write<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        bytes: &'a [u8],
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(self.effect(VcsCall::BundleWrite {
            repo: repo.to_owned(),
            id: id.to_owned(),
            offset,
            bytes: hex(bytes),
        }))
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
        Box::pin(self.effect(VcsCall::BundleApply {
            repo: repo.to_owned(),
            id: id.to_owned(),
            digest: digest.to_owned(),
            namespace: namespace.to_owned(),
            fetch: fetch.to_vec(),
            pins: pins.to_vec(),
        }))
    }

    fn ref_commit<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
    ) -> VcsFuture<'a, Result<Option<String>, VcsError>> {
        Box::pin(self.read(VcsCall::RefCommit {
            repo: repo.to_owned(),
            reference: reference.to_owned(),
        }))
    }

    fn dirty<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, bool> {
        Box::pin(async move {
            self.read(VcsCall::Dirty {
                path: path.to_owned(),
            })
            .await
            .unwrap_or(false)
        })
    }

    fn ahead_behind<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        Box::pin(async move {
            self.read(VcsCall::AheadBehind {
                child: child.to_owned(),
                parent: parent.to_owned(),
                base: base.map(str::to_owned),
            })
            .await
            .ok()
            .flatten()
        })
    }

    fn label<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Option<String>> {
        Box::pin(async move {
            self.read(VcsCall::Label {
                path: path.to_owned(),
            })
            .await
            .ok()
            .flatten()
        })
    }

    fn diverged_from_base<'a>(
        &'a self,
        path: &'a Path,
        base_change_id: Option<&'a str>,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        Box::pin(self.read(VcsCall::DivergedFromBase {
            path: path.to_owned(),
            base_change_id: base_change_id.map(str::to_owned),
            base_commit: base_commit.to_owned(),
        }))
    }

    fn copy_is_over_candidate<'a>(
        &'a self,
        launch: &'a Path,
        copy_id: &'a str,
        candidate: &'a VcsSnapshot,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        Box::pin(self.read(VcsCall::CopyIsOverCandidate {
            launch: launch.to_owned(),
            copy_id: copy_id.to_owned(),
            candidate: candidate.clone(),
        }))
    }
}

/// Which workspace paths the runner side answers about.
pub type VcsPathAdmission = dyn Fn(&Path) -> bool + Send + Sync;

/// The runner side: answers `vcs.read` and `vcs.effect` with the system
/// port over this machine's filesystem, for paths `admits` accepts. A call
/// naming any other path is refused unavailable, with no effect. Copies
/// that cross from another runner are minted from each tree's mirror under
/// `copies_root`; without one, this runner takes none.
#[must_use]
pub fn vcs_runner_extension(
    admits: Arc<VcsPathAdmission>,
    copies_root: Option<PathBuf>,
) -> Arc<dyn RunnerExtension> {
    Arc::new(VcsExtension {
        admits,
        copies_root,
    })
}

struct VcsExtension {
    admits: Arc<VcsPathAdmission>,
    copies_root: Option<PathBuf>,
}

impl RunnerExtension for VcsExtension {
    fn serve(&self, verb: RunnerVerb, payload: Value) -> ServeFuture {
        if !matches!(verb, RunnerVerb::VcsRead | RunnerVerb::VcsEffect) {
            return Box::pin(std::future::ready(Err(ServeFailure::Unsupported)));
        }
        let admits = Arc::clone(&self.admits);
        let copies_root = self.copies_root.clone();
        Box::pin(async move {
            let Ok(call) = serde_json::from_value::<VcsCall>(payload) else {
                return Err(ServeFailure::Unsupported);
            };
            // A question asked as an effect, or an effect asked as a
            // question, is not a call this side answers.
            if call.is_effect() != (verb == RunnerVerb::VcsEffect) {
                return Err(ServeFailure::Unsupported);
            }
            // Only an absolute path without `..` reaches the admission, so
            // a prefix check cannot be walked out of.
            let plain = |path: &Path| {
                path.is_absolute()
                    && !path
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
            };
            if !call
                .paths()
                .into_iter()
                .all(|path| plain(path) && admits(path))
            {
                return answer::<()>(Err(VcsError::mint_unavailable(
                    "the runner does not serve this workspace",
                )));
            }
            if let VcsCall::TransferRoot { runner_id, tree_id } = &call {
                return answer(transfer_root(copies_root.as_deref(), runner_id, tree_id));
            }
            serve(call).await
        })
    }
}

impl VcsCall {
    const fn is_effect(&self) -> bool {
        matches!(
            self,
            Self::Mint { .. }
                | Self::Seal { .. }
                | Self::Unpin { .. }
                | Self::RestoreCopyBase { .. }
                | Self::Adopt { .. }
                | Self::MintHook { .. }
                | Self::Unmint { .. }
                | Self::ApplyRefresh { .. }
                | Self::TrimDisposableTarget { .. }
                | Self::SnapshotBase { .. }
                | Self::BundleCreate { .. }
                | Self::BundleDrop { .. }
                | Self::BundleWrite { .. }
                | Self::BundleApply { .. }
                | Self::SweepTransfers { .. }
        )
    }
}

fn answer<T: Serialize>(result: Result<T, VcsError>) -> Result<Value, ServeFailure> {
    serde_json::to_value(result).map_err(|_| ServeFailure::Unsupported)
}

/// The mirror of tree `tree_id` under `copies_root`, keyed by the runner
/// that keeps it, so runners sharing a copies root keep apart mirrors.
fn transfer_root(
    copies_root: Option<&Path>,
    runner_id: &str,
    tree_id: &str,
) -> Result<PathBuf, VcsError> {
    let copies_root = copies_root.ok_or_else(|| {
        VcsError::mint_unavailable("this runner takes no copies from other runners")
    })?;
    let plain = |id: &str| {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    if !plain(runner_id) || !plain(tree_id) {
        return Err(VcsError::mint_unavailable("invalid runner or tree id"));
    }
    Ok(copies_root.join(runner_id).join(tree_id))
}

/// Runs one admitted call through the system port.
async fn serve(call: VcsCall) -> Result<Value, ServeFailure> {
    let vcs = SystemVcs;
    match call {
        VcsCall::PathExists { path } => answer(vcs.path_exists(&path).await),
        VcsCall::JjCopyDirs { launch } => answer(vcs.jj_copy_dirs(&launch).await),
        VcsCall::Label { path } => answer::<Option<String>>(Ok(vcs.label(&path).await)),
        VcsCall::Dirty { path } => answer::<bool>(Ok(vcs.dirty(&path).await)),
        VcsCall::AheadBehind {
            child,
            parent,
            base,
        } => answer::<Option<(u64, u64)>>(Ok(vcs
            .ahead_behind(&child, &parent, base.as_deref())
            .await)),
        VcsCall::DivergedFromBase {
            path,
            base_change_id,
            base_commit,
        } => answer(
            vcs.diverged_from_base(&path, base_change_id.as_deref(), &base_commit)
                .await,
        ),
        VcsCall::CopyIsOverCandidate {
            launch,
            copy_id,
            candidate,
        } => answer(
            vcs.copy_is_over_candidate(&launch, &copy_id, &candidate)
                .await,
        ),
        VcsCall::HistoryLeftBase { path, base, sealed } => {
            answer(vcs.history_left_base(&path, &base, &sealed).await)
        }
        VcsCall::CheckReclaimable { path, sealed } => {
            answer(vcs.check_reclaimable(&path, sealed.as_ref()).await)
        }
        VcsCall::PrepareRefresh { request } => answer(vcs.prepare_refresh(&request).await),
        VcsCall::Mint { request } => answer(vcs.mint_from(&request.request()).await),
        VcsCall::Seal {
            child_path,
            namespace,
            candidate_id,
            base_commit,
        } => answer(
            vcs.seal(&child_path, &namespace, &candidate_id, &base_commit)
                .await,
        ),
        VcsCall::Unpin {
            child_path,
            namespace,
            candidate_id,
        } => answer(vcs.unpin(&child_path, &namespace, &candidate_id).await),
        VcsCall::RestoreCopyBase {
            path,
            namespace,
            agent,
            base,
        } => answer(
            vcs.restore_copy_base(&path, &namespace, &agent, &base)
                .await,
        ),
        VcsCall::Adopt { request } => {
            answer::<Result<AdoptLanding, AdoptFailure>>(Ok(vcs.adopt(&request.request()).await))
        }
        VcsCall::MintHook { cwd, command, env } => {
            answer(vcs.run_mint_hook(&cwd, &command, &env).await)
        }
        VcsCall::Unmint {
            launch,
            path,
            namespace,
        } => answer(vcs.unmint(&launch, &path, &namespace).await),
        VcsCall::ApplyRefresh { plan, recover } => answer(vcs.apply_refresh(&plan, recover).await),
        VcsCall::TrimDisposableTarget { path } => answer(vcs.trim_disposable_target(&path).await),
        transfer => serve_transfer(transfer).await,
    }
}

/// [`serve`] for the calls that carry copies between runners.
async fn serve_transfer(call: VcsCall) -> Result<Value, ServeFailure> {
    let vcs = SystemVcs;
    match call {
        VcsCall::TransferRoot { .. } => answer::<PathBuf>(Err(VcsError::mint_unavailable(
            "this runner takes no copies from other runners",
        ))),
        VcsCall::SnapshotBase {
            parent_cwd,
            namespace,
            agent,
        } => answer(vcs.snapshot_base(&parent_cwd, &namespace, &agent).await),
        VcsCall::BundleCreate {
            repo,
            reference,
            exclude,
        } => answer(vcs.bundle_create(&repo, &reference, &exclude).await),
        VcsCall::BundleRead {
            repo,
            id,
            offset,
            len,
        } => answer(
            vcs.bundle_read(&repo, &id, offset, len)
                .await
                .map(|bytes| hex(&bytes)),
        ),
        VcsCall::BundleDrop { repo, id } => answer(vcs.bundle_drop(&repo, &id).await),
        VcsCall::BundleWrite {
            repo,
            id,
            offset,
            bytes,
        } => match unhex(&bytes) {
            Some(bytes) => answer(vcs.bundle_write(&repo, &id, offset, &bytes).await),
            None => Err(ServeFailure::Unsupported),
        },
        VcsCall::BundleApply {
            repo,
            id,
            digest,
            namespace,
            fetch,
            pins,
        } => answer(
            vcs.bundle_apply(&repo, &id, &digest, &namespace, &fetch, &pins)
                .await,
        ),
        VcsCall::RefCommit { repo, reference } => answer(vcs.ref_commit(&repo, &reference).await),
        VcsCall::SweepTransfers { repo, max_age_secs } => {
            answer(vcs.sweep_transfers(&repo, max_age_secs).await)
        }
        _ => Err(ServeFailure::Unsupported),
    }
}

#[cfg(test)]
mod tests;
