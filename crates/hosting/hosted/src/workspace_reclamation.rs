//! Host-owned admission retained across the real VCS removal or trim future.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use plakous_workspace::occupancy_vcs::VcsError;
use plakous_workspace::occupancy_vcs::{
    self as vcs, AdoptFailure, AdoptLanding, AdoptRequest, BundleFacts, MintRequest, MintedCopy,
    VcsFuture, VcsOps, VcsSnapshot,
};

/// An owned admission, released only when the destructive future ends or is dropped.
pub trait ReclamationAdmission: Send {}
impl<T: Send> ReclamationAdmission for T {}

/// Async paired-runner admission retained across a host-local destructive future.
pub type ReclamationFuture<'a> = VcsFuture<'a, Result<Box<dyn ReclamationAdmission>, VcsError>>;

/// The host's terminal-pin authority, independent of agent jobs and line ownership.
pub trait ReclamationFence: Send + Sync {
    fn admit<'a>(&'a self, runner: &'a str, path: &'a Path) -> ReclamationFuture<'a>;
}

pub(crate) struct GuardedVcs {
    inner: Arc<dyn VcsOps>,
    fence: Arc<dyn ReclamationFence>,
}

impl GuardedVcs {
    pub(crate) fn wrap(
        inner: Arc<dyn VcsOps>,
        fence: Arc<dyn ReclamationFence>,
    ) -> Arc<dyn VcsOps> {
        Arc::new(Self { inner, fence })
    }

    fn admit<'a>(&'a self, path: &'a Path) -> ReclamationFuture<'a> {
        self.fence
            .admit(self.inner.stage_runner().unwrap_or("local"), path)
    }
}

impl VcsOps for GuardedVcs {
    fn stage_runner(&self) -> Option<&str> {
        self.inner.stage_runner()
    }
    fn placement_ready(&self, path: &Path) -> Option<bool> {
        self.inner.placement_ready(path)
    }
    fn placement_held(&self, path: &Path) -> bool {
        self.inner.placement_held(path)
    }
    fn transfer_port(&self, runner: &str) -> Option<Arc<dyn VcsOps>> {
        self.inner
            .transfer_port(runner)
            .map(|inner| Self::wrap(inner, self.fence.clone()))
    }

    fn check_reclaimable<'a>(
        &'a self,
        path: &'a Path,
        sealed: Option<&'a VcsSnapshot>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(async move {
            let _admission = self.admit(path).await?;
            self.inner.check_reclaimable(path, sealed).await
        })
    }
    fn trim_disposable_target<'a>(
        &'a self,
        path: &'a Path,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        Box::pin(async move {
            let _admission = self.admit(path).await?;
            self.inner.trim_disposable_target(path).await
        })
    }
    fn unmint<'a>(
        &'a self,
        launch: &'a Path,
        path: &'a Path,
        namespace: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(async move {
            let _admission = self.admit(path).await?;
            self.inner.unmint(launch, path, namespace).await
        })
    }

    fn transfer_root<'a>(&'a self, tree_id: &'a str) -> VcsFuture<'a, Result<PathBuf, VcsError>> {
        self.inner.transfer_root(tree_id)
    }

    fn snapshot_base<'a>(
        &'a self,
        parent_cwd: &'a Path,
        namespace: &'a str,
        agent: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        self.inner.snapshot_base(parent_cwd, namespace, agent)
    }

    fn bundle_create<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
        exclude: &'a [String],
    ) -> VcsFuture<'a, Result<BundleFacts, VcsError>> {
        self.inner.bundle_create(repo, reference, exclude)
    }

    fn bundle_read<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        len: u64,
    ) -> VcsFuture<'a, Result<Vec<u8>, VcsError>> {
        self.inner.bundle_read(repo, id, offset, len)
    }

    fn bundle_drop<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.inner.bundle_drop(repo, id)
    }

    fn bundle_write<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        bytes: &'a [u8],
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.inner.bundle_write(repo, id, offset, bytes)
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
        self.inner
            .bundle_apply(repo, id, digest, namespace, fetch, pins)
    }

    fn ref_commit<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
    ) -> VcsFuture<'a, Result<Option<String>, VcsError>> {
        self.inner.ref_commit(repo, reference)
    }

    fn sweep_transfers<'a>(
        &'a self,
        repo: &'a Path,
        max_age_secs: u64,
    ) -> VcsFuture<'a, Result<u32, VcsError>> {
        self.inner.sweep_transfers(repo, max_age_secs)
    }

    fn prepare_refresh<'a>(
        &'a self,
        request: &'a vcs::RefreshRequest,
    ) -> VcsFuture<'a, Result<vcs::RefreshPreparation, VcsError>> {
        self.inner.prepare_refresh(request)
    }

    fn apply_refresh<'a>(
        &'a self,
        plan: &'a vcs::RefreshPlan,
        recover: bool,
    ) -> VcsFuture<'a, Result<vcs::RefreshApplied, VcsError>> {
        self.inner.apply_refresh(plan, recover)
    }

    fn path_exists<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Result<bool, VcsError>> {
        self.inner.path_exists(path)
    }

    fn jj_copy_dirs<'a>(
        &'a self,
        launch: &'a Path,
    ) -> VcsFuture<'a, Result<BTreeSet<String>, VcsError>> {
        self.inner.jj_copy_dirs(launch)
    }

    fn mint_from<'a>(
        &'a self,
        request: &'a MintRequest<'_>,
    ) -> VcsFuture<'a, Result<MintedCopy, VcsError>> {
        self.inner.mint_from(request)
    }

    fn history_left_base<'a>(
        &'a self,
        path: &'a Path,
        base: &'a str,
        sealed: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        self.inner.history_left_base(path, base, sealed)
    }

    fn seal<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        self.inner
            .seal(child_path, namespace, candidate_id, base_commit)
    }

    fn unpin<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.inner.unpin(child_path, namespace, candidate_id)
    }

    fn restore_copy_base<'a>(
        &'a self,
        path: &'a Path,
        namespace: &'a str,
        agent: &'a str,
        base: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.inner.restore_copy_base(path, namespace, agent, base)
    }

    fn adopt<'a>(
        &'a self,
        request: &'a AdoptRequest<'_>,
    ) -> VcsFuture<'a, Result<AdoptLanding, AdoptFailure>> {
        self.inner.adopt(request)
    }

    fn run_mint_hook<'a>(
        &'a self,
        cwd: &'a Path,
        command: &'a str,
        env: &'a BTreeMap<String, String>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.inner.run_mint_hook(cwd, command, env)
    }

    fn dirty<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, bool> {
        self.inner.dirty(path)
    }

    fn ahead_behind<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        self.inner.ahead_behind(child, parent, base)
    }

    fn ahead_behind_cached<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
        cache: &'a mut vcs::ParentBehindCache,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        self.inner.ahead_behind_cached(child, parent, base, cache)
    }

    fn label<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Option<String>> {
        self.inner.label(path)
    }

    fn diverged_from_base<'a>(
        &'a self,
        path: &'a Path,
        base_change_id: Option<&'a str>,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        self.inner
            .diverged_from_base(path, base_change_id, base_commit)
    }

    fn copy_is_over_candidate<'a>(
        &'a self,
        launch: &'a Path,
        copy_id: &'a str,
        candidate: &'a VcsSnapshot,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        self.inner
            .copy_is_over_candidate(launch, copy_id, candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Admission(Arc<AtomicBool>);
    impl Drop for Admission {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    struct Fence {
        allowed: AtomicBool,
        held: Arc<AtomicBool>,
    }
    impl ReclamationFence for Fence {
        fn admit<'a>(&'a self, _: &'a str, _: &'a Path) -> ReclamationFuture<'a> {
            Box::pin(async move {
                if !self.allowed.load(Ordering::SeqCst) {
                    return Err(VcsError::mint_unavailable(
                        "operator terminal retains workspace",
                    ));
                }
                assert!(!self.held.swap(true, Ordering::SeqCst));
                Ok(Box::new(Admission(self.held.clone())) as Box<dyn ReclamationAdmission>)
            })
        }
    }

    #[tokio::test]
    async fn operator_host_reclamation_admission_spans_the_actual_removal() {
        let held = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(Fence {
            allowed: AtomicBool::new(false),
            held: held.clone(),
        });
        let inner = Arc::new(vcs::fake::FakeVcs::new());
        let probe = held.clone();
        inner.watch_lock(move || probe.load(Ordering::SeqCst));
        let guarded = GuardedVcs::wrap(inner.clone(), fence.clone());
        let path = Path::new("/workspace/child");
        assert!(guarded.check_reclaimable(path, None).await.is_err());
        assert!(guarded.trim_disposable_target(path).await.is_err());
        assert!(
            guarded
                .unmint(Path::new("/workspace"), path, "tree")
                .await
                .is_err()
        );
        assert!(inner.calls().is_empty(), "a pin refuses before any removal");
        fence.allowed.store(true, Ordering::SeqCst);
        assert!(
            guarded
                .unmint(Path::new("/workspace"), path, "tree")
                .await
                .is_ok()
        );
        assert!(inner.calls().iter().all(|call| call.lock_held));
        assert_eq!(inner.calls().len(), 1);
        assert!(
            !held.load(Ordering::SeqCst),
            "completion releases admission"
        );
    }
}
