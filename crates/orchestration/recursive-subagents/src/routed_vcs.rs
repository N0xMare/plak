//! A tree's VCS port across runners (chapter 03 §14): the port of the
//! runner the tree launched on, and the ports of the other runners its
//! copies may cross to. A call about a path under a runner's mirror of the
//! tree goes to that runner's port, with the mirror as its launch checkout;
//! any other call goes to the launch runner's port.
//!
//! Each runner's mirror is `<copies root>/<runner id>/<tree id>` on that
//! runner; a runner that names none takes no copies and is left out, and
//! two runners whose mirrors nest are both left out.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous_workspace::occupancy_vcs::{
    AdoptFailure, AdoptLanding, AdoptRequest, BundleFacts, MintRequest, MintedCopy,
    ParentBehindCache, RefreshApplied, RefreshPlan, RefreshPreparation, RefreshRequest, VcsError,
    VcsFuture, VcsOps, VcsSnapshot,
};

/// One other runner's port and the tree's mirror on it.
struct Route {
    runner: String,
    port: Arc<dyn VcsOps>,
    mirror: PathBuf,
}

/// The tree's port over its launch runner and the runners its copies may
/// cross to.
pub struct RoutedVcs {
    home: Arc<dyn VcsOps>,
    routes: Vec<Route>,
}

impl RoutedVcs {
    /// The port of tree `tree_id` over `home`, the launch runner's port,
    /// and `runners`, each another runner's port. Each runner is asked
    /// for its mirror of the tree once; one that names none, or names no
    /// runner, is left out.
    pub async fn new(home: Arc<dyn VcsOps>, tree_id: &str, runners: Vec<Arc<dyn VcsOps>>) -> Self {
        let mut routes = Vec::new();
        for port in runners {
            let Some(runner) = port.stage_runner().map(str::to_owned) else {
                continue;
            };
            if home.stage_runner() == Some(runner.as_str()) {
                continue;
            }
            let Ok(mirror) = port.transfer_root(tree_id).await else {
                continue;
            };
            // A path names one runner only when no two mirrors nest: two
            // runners whose mirrors overlap are both left out rather than
            // one answering for the other's copies.
            if let Some(index) = routes.iter().position(|route: &Route| {
                route.mirror.starts_with(&mirror) || mirror.starts_with(&route.mirror)
            }) {
                routes.remove(index);
                continue;
            }
            routes.push(Route {
                runner,
                port,
                mirror,
            });
        }
        Self { home, routes }
    }

    fn route(&self, path: &Path) -> Option<&Route> {
        self.routes
            .iter()
            .find(|route| path.starts_with(&route.mirror))
    }

    /// The port that answers about `path`.
    fn port(&self, path: &Path) -> &dyn VcsOps {
        self.route(path)
            .map_or(self.home.as_ref(), |route| route.port.as_ref())
    }

    /// The port that answers about `path`, and the launch checkout it
    /// takes there: `launch` on the launch runner, the mirror elsewhere.
    fn port_launch<'a>(&'a self, path: &Path, launch: &'a Path) -> (&'a dyn VcsOps, &'a Path) {
        self.route(path)
            .map_or((self.home.as_ref(), launch), |route| {
                (route.port.as_ref(), route.mirror.as_path())
            })
    }

    /// Whether `a` and `b` are answered by the same port.
    fn same_port(&self, a: &Path, b: &Path) -> bool {
        self.route(a).map(|route| &route.runner) == self.route(b).map(|route| &route.runner)
    }
}

fn ready<'a, T: Send + 'a>(value: T) -> VcsFuture<'a, T> {
    Box::pin(std::future::ready(value))
}

impl VcsOps for RoutedVcs {
    fn stage_runner(&self) -> Option<&str> {
        self.home.stage_runner()
    }

    fn transfer_port(&self, runner: &str) -> Option<Arc<dyn VcsOps>> {
        self.routes
            .iter()
            .find(|route| route.runner == runner)
            .map(|route| Arc::clone(&route.port))
    }

    fn transfer_root<'a>(&'a self, tree_id: &'a str) -> VcsFuture<'a, Result<PathBuf, VcsError>> {
        self.home.transfer_root(tree_id)
    }

    fn snapshot_base<'a>(
        &'a self,
        parent_cwd: &'a Path,
        namespace: &'a str,
        agent: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        self.port(parent_cwd)
            .snapshot_base(parent_cwd, namespace, agent)
    }

    fn bundle_create<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
        exclude: &'a [String],
    ) -> VcsFuture<'a, Result<BundleFacts, VcsError>> {
        self.port(repo).bundle_create(repo, reference, exclude)
    }

    fn bundle_read<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        len: u64,
    ) -> VcsFuture<'a, Result<Vec<u8>, VcsError>> {
        self.port(repo).bundle_read(repo, id, offset, len)
    }

    fn bundle_drop<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.port(repo).bundle_drop(repo, id)
    }

    fn bundle_write<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        bytes: &'a [u8],
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.port(repo).bundle_write(repo, id, offset, bytes)
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
        self.port(repo)
            .bundle_apply(repo, id, digest, namespace, fetch, pins)
    }

    fn ref_commit<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
    ) -> VcsFuture<'a, Result<Option<String>, VcsError>> {
        self.port(repo).ref_commit(repo, reference)
    }

    /// Sweeps `repo` on the launch runner and, for the launch checkout,
    /// every runner's mirror of the tree as well.
    fn sweep_transfers<'a>(
        &'a self,
        repo: &'a Path,
        max_age_secs: u64,
    ) -> VcsFuture<'a, Result<u32, VcsError>> {
        Box::pin(async move {
            let mut removed = self.port(repo).sweep_transfers(repo, max_age_secs).await?;
            if self.route(repo).is_none() {
                for route in &self.routes {
                    removed += route
                        .port
                        .sweep_transfers(&route.mirror, max_age_secs)
                        .await?;
                }
            }
            Ok(removed)
        })
    }

    fn prepare_refresh<'a>(
        &'a self,
        request: &'a RefreshRequest,
    ) -> VcsFuture<'a, Result<RefreshPreparation, VcsError>> {
        if self.route(&request.child).is_some() || self.route(&request.parent).is_some() {
            return ready(Ok(RefreshPreparation::Skipped(
                "a copy on another runner than the tree's launch runner is not refreshed".into(),
            )));
        }
        self.home.prepare_refresh(request)
    }

    fn apply_refresh<'a>(
        &'a self,
        plan: &'a RefreshPlan,
        recover: bool,
    ) -> VcsFuture<'a, Result<RefreshApplied, VcsError>> {
        self.port(&plan.request.child).apply_refresh(plan, recover)
    }

    fn check_reclaimable<'a>(
        &'a self,
        path: &'a Path,
        sealed: Option<&'a VcsSnapshot>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.port(path).check_reclaimable(path, sealed)
    }

    fn trim_disposable_target<'a>(
        &'a self,
        path: &'a Path,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        self.port(path).trim_disposable_target(path)
    }

    fn path_exists<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Result<bool, VcsError>> {
        self.port(path).path_exists(path)
    }

    fn jj_copy_dirs<'a>(
        &'a self,
        launch: &'a Path,
    ) -> VcsFuture<'a, Result<BTreeSet<String>, VcsError>> {
        self.home.jj_copy_dirs(launch)
    }

    fn mint_from<'a>(
        &'a self,
        request: &'a MintRequest<'_>,
    ) -> VcsFuture<'a, Result<MintedCopy, VcsError>> {
        let (port, launch) = self.port_launch(request.parent_cwd, request.launch);
        Box::pin(async move {
            port.mint_from(&MintRequest {
                launch,
                agent_id: request.agent_id,
                namespace: request.namespace,
                parent_cwd: request.parent_cwd,
                mode: request.mode,
                base: request.base,
            })
            .await
        })
    }

    fn history_left_base<'a>(
        &'a self,
        path: &'a Path,
        base: &'a str,
        sealed: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        self.port(path).history_left_base(path, base, sealed)
    }

    fn seal<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        self.port(child_path)
            .seal(child_path, namespace, candidate_id, base_commit)
    }

    fn unpin<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.port(child_path)
            .unpin(child_path, namespace, candidate_id)
    }

    fn restore_copy_base<'a>(
        &'a self,
        path: &'a Path,
        namespace: &'a str,
        agent: &'a str,
        base: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.port(path)
            .restore_copy_base(path, namespace, agent, base)
    }

    fn adopt<'a>(
        &'a self,
        request: &'a AdoptRequest<'_>,
    ) -> VcsFuture<'a, Result<AdoptLanding, AdoptFailure>> {
        self.port(request.target_path).adopt(request)
    }

    fn run_mint_hook<'a>(
        &'a self,
        cwd: &'a Path,
        command: &'a str,
        env: &'a BTreeMap<String, String>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        self.port(cwd).run_mint_hook(cwd, command, env)
    }

    fn unmint<'a>(
        &'a self,
        launch: &'a Path,
        path: &'a Path,
        namespace: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (port, launch) = self.port_launch(path, launch);
        port.unmint(launch, path, namespace)
    }

    fn dirty<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, bool> {
        self.port(path).dirty(path)
    }

    fn ahead_behind<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        if !self.same_port(child, parent) {
            return ready(None);
        }
        self.port(child).ahead_behind(child, parent, base)
    }

    fn ahead_behind_cached<'a>(
        &'a self,
        child: &'a Path,
        parent: &'a Path,
        base: Option<&'a str>,
        cache: &'a mut ParentBehindCache,
    ) -> VcsFuture<'a, Option<(u64, u64)>> {
        if !self.same_port(child, parent) {
            return ready(None);
        }
        self.port(child)
            .ahead_behind_cached(child, parent, base, cache)
    }

    fn label<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Option<String>> {
        self.port(path).label(path)
    }

    fn diverged_from_base<'a>(
        &'a self,
        path: &'a Path,
        base_change_id: Option<&'a str>,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        self.port(path)
            .diverged_from_base(path, base_change_id, base_commit)
    }

    fn copy_is_over_candidate<'a>(
        &'a self,
        launch: &'a Path,
        copy_id: &'a str,
        candidate: &'a VcsSnapshot,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        self.home.copy_is_over_candidate(launch, copy_id, candidate)
    }
}
