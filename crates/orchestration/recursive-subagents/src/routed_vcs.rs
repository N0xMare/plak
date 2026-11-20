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
use std::sync::{Arc, PoisonError, RwLock};

use plakous_workspace::occupancy_vcs::{
    AdoptFailure, AdoptLanding, AdoptRequest, BundleFacts, MintRequest, MintedCopy,
    ParentBehindCache, RefreshApplied, RefreshPlan, RefreshPreparation, RefreshRequest, VcsError,
    VcsFuture, VcsOps, VcsSnapshot,
};

/// One other runner's port and the tree's mirror on it.
#[derive(Clone)]
struct Route {
    runner: String,
    port: Arc<dyn VcsOps>,
    mirror: PathBuf,
}

type ResolveRunner = dyn Fn(&str) -> Arc<dyn VcsOps> + Send + Sync;

/// The tree's port over its launch runner and the runners its copies may
/// cross to.
pub struct RoutedVcs {
    home: Arc<dyn VcsOps>,
    routes: RwLock<Vec<Route>>,
    resolver: Option<Box<ResolveRunner>>,
    tree_id: String,
    launch: Option<PathBuf>,
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
        Self {
            home,
            routes: RwLock::new(routes),
            resolver: None,
            tree_id: tree_id.to_owned(),
            launch: None,
        }
    }

    /// Resolves transfer ports on demand through the host's paired services.
    /// Saved places must be installed before exposing this port to recovery.
    #[must_use]
    pub fn with_resolver(
        home: Arc<dyn VcsOps>,
        tree_id: &str,
        launch: PathBuf,
        resolver: impl Fn(&str) -> Arc<dyn VcsOps> + Send + Sync + 'static,
    ) -> Self {
        Self {
            home,
            routes: RwLock::default(),
            resolver: Some(Box::new(resolver)),
            tree_id: tree_id.to_owned(),
            launch: Some(launch),
        }
    }

    /// Retains the route for a saved cross-runner checkout without requiring
    /// the runner to answer during root reopen.
    ///
    /// # Errors
    /// Refuses a conflicting mirror or a path outside the recorded mirror layout.
    pub fn restore_route(&self, runner: &str, path: &Path) -> Result<(), VcsError> {
        let Some(resolver) = &self.resolver else {
            return Err(VcsError::mint_unavailable("runner routing is unavailable"));
        };
        // A transferred copy is <mirror>/.plak/worktrees/<copy>. The
        // runner and tree components distinguish it from the launch checkout.
        let mirror = path.ancestors().nth(3).filter(|mirror| {
            mirror.file_name().and_then(|name| name.to_str()) == Some(self.tree_id.as_str())
                && mirror
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    == Some(runner)
                && path
                    .strip_prefix(mirror)
                    .is_ok_and(|suffix| suffix.starts_with(".plak/worktrees"))
        });
        let Some(mirror) = mirror else {
            if self.home.stage_runner() == Some(runner) {
                return Ok(());
            }
            return Err(VcsError::mint_unavailable(
                "saved copy is outside its runner's tree mirror",
            ));
        };
        self.register_route(runner, resolver(runner), mirror.to_owned())
    }

    fn register_route(
        &self,
        runner: &str,
        port: Arc<dyn VcsOps>,
        mirror: PathBuf,
    ) -> Result<(), VcsError> {
        let overlaps = |other: &Path| mirror.starts_with(other) || other.starts_with(&mirror);
        let mut routes = self.routes.write().unwrap_or_else(PoisonError::into_inner);
        if self.launch.as_deref().is_some_and(overlaps)
            || routes.iter().any(|route| {
                (route.runner != runner || route.mirror != mirror) && overlaps(&route.mirror)
            })
        {
            return Err(VcsError::mint_unavailable(
                "runner mirror overlaps another checkout in this tree",
            ));
        }
        if let Some(index) = routes
            .iter()
            .position(|route| route.runner == runner && route.mirror == mirror)
        {
            routes.remove(index);
        }
        routes.push(Route {
            runner: runner.to_owned(),
            port,
            mirror,
        });
        Ok(())
    }

    fn route(&self, path: &Path) -> Option<Route> {
        self.routes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|route| path.starts_with(&route.mirror))
            .cloned()
    }

    /// The port that answers about `path`.
    fn port(&self, path: &Path) -> Arc<dyn VcsOps> {
        self.route(path)
            .map_or_else(|| Arc::clone(&self.home), |route| route.port)
    }

    /// The port that answers about `path`, and the launch checkout it
    /// takes there: `launch` on the launch runner, the mirror elsewhere.
    fn port_launch(&self, path: &Path, launch: &Path) -> (Arc<dyn VcsOps>, PathBuf) {
        self.route(path).map_or_else(
            || (Arc::clone(&self.home), launch.to_owned()),
            |route| (route.port, route.mirror),
        )
    }

    /// Whether `a` and `b` are answered by the same port.
    fn same_port(&self, a: &Path, b: &Path) -> bool {
        self.path_runner(a) == self.path_runner(b)
    }
}

fn ready<'a, T: Send + 'a>(value: T) -> VcsFuture<'a, T> {
    Box::pin(std::future::ready(value))
}

impl VcsOps for RoutedVcs {
    fn placement_ready(&self, path: &Path) -> Option<bool> {
        self.port(path).placement_ready(path)
    }

    fn placement_held(&self, path: &Path) -> bool {
        self.port(path).placement_held(path)
    }

    fn stage_runner(&self) -> Option<&str> {
        self.home.stage_runner()
    }

    fn path_runner(&self, path: &Path) -> Option<String> {
        self.route(path)
            .map(|route| route.runner)
            .or_else(|| self.stage_runner().map(str::to_owned))
    }

    fn prepare_transfer<'a>(&'a self, runner: &'a str) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(async move {
            if let Some(resolve) = &self.resolver {
                let port = resolve(runner);
                let mirror = port.transfer_root(&self.tree_id).await?;
                return self.register_route(runner, port, mirror);
            }
            self.transfer_port(runner).map(|_| ()).ok_or_else(|| {
                VcsError::mint_unavailable("the tree's port does not reach that runner")
            })
        })
    }

    fn transfer_port(&self, runner: &str) -> Option<Arc<dyn VcsOps>> {
        self.routes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find(|route| route.runner == runner)
            .map(|route| Arc::clone(&route.port))
    }

    fn transfer_mirror(&self, runner: &str) -> Option<PathBuf> {
        self.routes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find(|route| route.runner == runner)
            .map(|route| route.mirror.clone())
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
        Box::pin(async move {
            self.port(parent_cwd)
                .snapshot_base(parent_cwd, namespace, agent)
                .await
        })
    }

    fn bundle_create<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
        exclude: &'a [String],
    ) -> VcsFuture<'a, Result<BundleFacts, VcsError>> {
        Box::pin(async move {
            self.port(repo)
                .bundle_create(repo, reference, exclude)
                .await
        })
    }

    fn bundle_read<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        len: u64,
    ) -> VcsFuture<'a, Result<Vec<u8>, VcsError>> {
        Box::pin(async move { self.port(repo).bundle_read(repo, id, offset, len).await })
    }

    fn bundle_drop<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(async move { self.port(repo).bundle_drop(repo, id).await })
    }

    fn bundle_write<'a>(
        &'a self,
        repo: &'a Path,
        id: &'a str,
        offset: u64,
        bytes: &'a [u8],
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(async move { self.port(repo).bundle_write(repo, id, offset, bytes).await })
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
        Box::pin(async move {
            self.port(repo)
                .bundle_apply(repo, id, digest, namespace, fetch, pins)
                .await
        })
    }

    fn ref_commit<'a>(
        &'a self,
        repo: &'a Path,
        reference: &'a str,
    ) -> VcsFuture<'a, Result<Option<String>, VcsError>> {
        Box::pin(async move { self.port(repo).ref_commit(repo, reference).await })
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
                let routes = self
                    .routes
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                for route in routes {
                    if route.port.placement_ready(&route.mirror) == Some(false) {
                        continue;
                    }
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
        Box::pin(async move {
            self.port(&plan.request.child)
                .apply_refresh(plan, recover)
                .await
        })
    }

    fn check_reclaimable<'a>(
        &'a self,
        path: &'a Path,
        sealed: Option<&'a VcsSnapshot>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(async move { self.port(path).check_reclaimable(path, sealed).await })
    }

    fn trim_disposable_target<'a>(
        &'a self,
        path: &'a Path,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        Box::pin(async move { self.port(path).trim_disposable_target(path).await })
    }

    fn path_exists<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Result<bool, VcsError>> {
        Box::pin(async move { self.port(path).path_exists(path).await })
    }

    fn jj_copy_dirs<'a>(
        &'a self,
        launch: &'a Path,
    ) -> VcsFuture<'a, Result<BTreeSet<String>, VcsError>> {
        let (port, launch) = self.port_launch(launch, launch);
        Box::pin(async move { port.jj_copy_dirs(&launch).await })
    }

    fn mint_from<'a>(
        &'a self,
        request: &'a MintRequest<'_>,
    ) -> VcsFuture<'a, Result<MintedCopy, VcsError>> {
        let (port, launch) = self.port_launch(request.parent_cwd, request.launch);
        Box::pin(async move {
            port.mint_from(&MintRequest {
                launch: &launch,
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
        Box::pin(async move { self.port(path).history_left_base(path, base, sealed).await })
    }

    fn seal<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
        Box::pin(async move {
            self.port(child_path)
                .seal(child_path, namespace, candidate_id, base_commit)
                .await
        })
    }

    fn unpin<'a>(
        &'a self,
        child_path: &'a Path,
        namespace: &'a str,
        candidate_id: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(async move {
            self.port(child_path)
                .unpin(child_path, namespace, candidate_id)
                .await
        })
    }

    fn restore_copy_base<'a>(
        &'a self,
        path: &'a Path,
        namespace: &'a str,
        agent: &'a str,
        base: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(async move {
            self.port(path)
                .restore_copy_base(path, namespace, agent, base)
                .await
        })
    }

    fn adopt<'a>(
        &'a self,
        request: &'a AdoptRequest<'_>,
    ) -> VcsFuture<'a, Result<AdoptLanding, AdoptFailure>> {
        Box::pin(async move {
            let port = self.port(request.target_path);
            if self.same_port(request.child_path, request.target_path) {
                return port.adopt(request).await;
            }
            // The candidate's objects and pins are already at the landing
            // repository. Do not send another runner's path or advance its pin.
            port.adopt(&AdoptRequest {
                child_path: request.target_path,
                source_agent_id: None,
                ..*request
            })
            .await
        })
    }

    fn run_mint_hook<'a>(
        &'a self,
        cwd: &'a Path,
        command: &'a str,
        env: &'a BTreeMap<String, String>,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        Box::pin(async move { self.port(cwd).run_mint_hook(cwd, command, env).await })
    }

    fn unmint<'a>(
        &'a self,
        launch: &'a Path,
        path: &'a Path,
        namespace: &'a str,
    ) -> VcsFuture<'a, Result<(), VcsError>> {
        let (port, launch) = self.port_launch(path, launch);
        Box::pin(async move { port.unmint(&launch, path, namespace).await })
    }

    fn dirty<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, bool> {
        Box::pin(async move { self.port(path).dirty(path).await })
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
        Box::pin(async move { self.port(child).ahead_behind(child, parent, base).await })
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
        Box::pin(async move {
            self.port(child)
                .ahead_behind_cached(child, parent, base, cache)
                .await
        })
    }

    fn label<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Option<String>> {
        Box::pin(async move { self.port(path).label(path).await })
    }

    fn diverged_from_base<'a>(
        &'a self,
        path: &'a Path,
        base_change_id: Option<&'a str>,
        base_commit: &'a str,
    ) -> VcsFuture<'a, Result<bool, VcsError>> {
        Box::pin(async move {
            self.port(path)
                .diverged_from_base(path, base_change_id, base_commit)
                .await
        })
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
