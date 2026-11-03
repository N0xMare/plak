//! The tree driver: one tree, its store and its VCS port, and the effects it settles.

use super::*;

/// One tree with its store and its VCS port: the engine every tree mutation
/// runs on. [`Self::mutate`] queues one writer and applies; the effects the
/// mutation queued come out with the result and [`Self::settle`] runs them
/// with the mutex released, writing once more to record them and publish.
/// [`Self::save`] is the fenced whole-tree publish. The guard a caller sees
/// inside `mutate` is the tree alone — it offers no VCS, so a subprocess
/// under the lock is not expressible here.
///
/// Writers run one at a time in the order they asked, each on a blocking
/// worker; a capacity-refused publication parks in the async caller, raced
/// against its agent's cancellation and the tree's shutdown. [`Self::read`]
/// and [`Self::try_read`] read the live tree throughout.
#[derive(Clone)]
pub struct TreeDriver {
    /// The tree under its one mutex.
    ///
    /// `pub(super)` and not `pub`: the tree's own tools interleave locked and
    /// unlocked phases deliberately and are reviewed as such, but a caller
    /// above this crate that locked the tree could mutate it and drop the
    /// guard without ever taking the [`Effects`] that mutation queued — a
    /// stranded seal or adopt, which reopens `RecoveryRequired`. Outside
    /// callers get [`Self::read`] and [`Self::mutate`] instead, and the
    /// mutation path cannot lose its effects because it returns them.
    pub(super) tree: Arc<Mutex<ProductAgentTree>>,
    pub(super) store: Arc<dyn TreeStore>,
    pub(super) vcs: Arc<dyn VcsOps>,
    pub(super) job_host: Arc<Mutex<Option<Arc<dyn plakous::tools::JobHost>>>>,
    retirement_recovery: Arc<Mutex<Option<RetirementRecovery>>>,
    retirement_tasks: Arc<Mutex<Vec<crate::task_set::TaskHandle<()>>>>,
    /// The tree's own tasks: they outlive any one root's release.
    tasks: crate::task_set::TaskSet,
    /// Work that owns a child's authority, kept apart from `tasks`; see
    /// [`Self::spawn_release_owner`].
    release_owners: crate::task_set::TaskSet,
    pub(crate) retirement_faults: tokio::sync::broadcast::Sender<(String, String)>,
    pub(super) observer: Option<Arc<dyn TopologyObserver>>,
    next_reclaim_probe: Arc<Mutex<u64>>,
    /// Whether the tree's store is refusing publications for capacity.
    storage_waiting: Arc<tokio::sync::watch::Sender<bool>>,
    // Readers must not infer admission state from a busy tree mutex. Every
    // mutation guard installs this predicate before releasing the live tree.
    restored_paused: Arc<std::sync::atomic::AtomicBool>,
    writer: Arc<storage::TreeWriter>,
    storage_parking: Arc<storage::StorageParking>,
    /// The agent a publication waits for when no other owner is named.
    root_agent_id: Arc<str>,
    /// The shutdown of the host that attached this tree last.
    host_shutdown: Arc<Mutex<Option<HostShutdown>>>,
    /// This driver's custody of the tree in its store: every save presents
    /// its claim, which the store proves in the write's own transaction. It
    /// is released when the last clone of this driver drops, so every writer
    /// that can still write (an owed release, a waiter that captured the
    /// driver) writes under it.
    custody: Arc<DriverCustody>,
    publication_owner: Option<String>,
    capacity_retry_interval: std::time::Duration,
    checkout_sizes: Arc<Mutex<std::collections::BTreeMap<String, CheckoutSize>>>,
}

type RetirementRecovery = Arc<dyn Fn() + Send + Sync>;

mod custody;
use custody::DriverCustody;
mod host_shutdown;
pub use host_shutdown::HostShutdown;

/// One writer turn's work, with its result already handed off.
type ErasedWork = Box<dyn FnOnce(&mut storage::TreeMutationGuard<'_>) + Send>;

/// How a writer waits for its turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Turn {
    /// Gives up while queued once its publication would be refused anyway.
    Abandonable,
    /// Owed work: waits for its turn whatever was cancelled.
    Owed,
}

struct CheckoutSize {
    generation: u64,
    trim_epoch: u64,
    path: std::path::PathBuf,
    measured_at: u64,
    bytes: u64,
}

impl CheckoutSize {
    fn reusable(
        &self,
        path: &std::path::Path,
        generation: u64,
        now: u64,
        retry_at: u64,
        trim_epoch: u64,
    ) -> bool {
        self.path == path
            && self.generation == generation
            && self.trim_epoch == trim_epoch
            && (now < retry_at || now.saturating_sub(self.measured_at) < 30_000)
    }
}

fn probe_checkout_sizes(
    cache: &mut std::collections::BTreeMap<String, CheckoutSize>,
    inventory: Vec<(String, std::path::PathBuf, u64, u64, u64)>,
    now: u64,
    mut walk: impl FnMut(&std::path::Path) -> Result<Option<u64>, plakous::tools::ProbeError>,
) -> Result<std::collections::BTreeMap<String, u64>, plakous::tools::ProbeError> {
    cache.retain(|id, _| inventory.iter().any(|(current, ..)| current == id));
    let mut sizes = std::collections::BTreeMap::new();
    for (id, path, generation, retry_at, trim_epoch) in inventory {
        if !cache
            .get(&id)
            .is_some_and(|size| size.reusable(&path, generation, now, retry_at, trim_epoch))
        {
            // An unmeasured copy is left out; a probe that cannot answer
            // ends the pass rather than reclaim on partial sizes.
            let Some(bytes) = walk(&path)? else {
                continue;
            };
            cache.insert(
                id.clone(),
                CheckoutSize {
                    generation,
                    trim_epoch,
                    path,
                    measured_at: now,
                    bytes,
                },
            );
        }
        if let Some(size) = cache.get(&id) {
            sizes.insert(id, size.bytes);
        }
    }
    Ok(sizes)
}

impl TreeDriver {
    #[cfg(test)]
    pub(super) fn capacity_retry_without_delay(&mut self) {
        self.capacity_retry_interval = std::time::Duration::ZERO;
    }

    /// Runs `work` as this tree's writer on the calling thread, for a test
    /// with no other writer. A capacity refusal inside it is not parked.
    ///
    /// # Panics
    ///
    /// Panics when another writer holds the tree or the tree is poisoned.
    #[cfg(test)]
    pub(crate) fn locked_for_test<R>(
        &self,
        work: impl FnOnce(&mut storage::TreeMutationGuard<'_>) -> R,
    ) -> R {
        let _permit = self
            .writer
            .order()
            .try_lock_owned()
            .unwrap_or_else(|_| panic!("a synchronous test runs with no other writer"));
        let mut tree = storage::TreeMutationGuard::lock(&self.tree, &self.restored_paused)
            .unwrap_or_else(|()| panic!("topology tree lock is poisoned"));
        work(&mut tree)
    }

    /// Binds `tree` to `store` and `vcs`, installing `vcs` as the tree's own
    /// port so the mint inside `spawn_agent` and the effects run here drive
    /// one implementation. A reopened tree's base-pin and refresh
    /// reconciliation waits for [`Self::reconcile_attachment`].
    pub fn new(
        tree: Arc<Mutex<ProductAgentTree>>,
        store: Arc<dyn TreeStore>,
        vcs: Arc<dyn VcsOps>,
        observer: Option<Arc<dyn TopologyObserver>>,
    ) -> Self {
        // Custody precedes the tree's first save here, and every save after:
        // a write without it is refused before it reaches the store.
        let custody = DriverCustody::take(&tree, &store);
        if let Ok(mut guard) = tree.lock() {
            guard.set_vcs(Arc::clone(&vcs));
        }
        let (restored_paused, root_agent_id) = {
            let tree = tree
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                tree.state == crate::tree::TreeAdmissionState::RestoredPaused,
                Arc::from(tree.root_agent_id.as_str()),
            )
        };
        Self {
            tree,
            store,
            vcs,
            job_host: Arc::new(Mutex::new(None)),
            retirement_recovery: Arc::new(Mutex::new(None)),
            retirement_tasks: Arc::default(),
            tasks: crate::task_set::TaskSet::new(),
            release_owners: crate::task_set::TaskSet::new(),
            retirement_faults: tokio::sync::broadcast::channel(64).0,
            observer,
            next_reclaim_probe: Arc::new(Mutex::new(0)),
            storage_waiting: Arc::new(tokio::sync::watch::Sender::new(false)),
            restored_paused: Arc::new(std::sync::atomic::AtomicBool::new(restored_paused)),
            writer: Arc::default(),
            storage_parking: Arc::default(),
            root_agent_id,
            host_shutdown: Arc::default(),
            custody: Arc::new(custody),
            publication_owner: None,
            capacity_retry_interval: std::time::Duration::from_secs(5),
            checkout_sizes: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
        }
    }

    /// Reclaims only acknowledged, settled dormant copies. Size probes run on
    /// blocking workers and VCS work through the port, never under the tree
    /// mutex.
    pub async fn reclaim_dormant_checkouts(&self) -> Result<(), TopologyError> {
        let now = plakous::model::now_unix_millis();
        let delay = self.read(|tree| tree.current_epoch().workspace_idle_seconds)?;
        {
            let mut next = self
                .next_reclaim_probe
                .lock()
                .map_err(|_| Self::lock_poisoned())?;
            if now < *next {
                return Ok(());
            }
            *next = now.saturating_add(u64::from(delay.min(30)) * 1000);
        }
        let inventory = self.read(ProductAgentTree::checkout_size_probes)?;
        let cache = Arc::clone(&self.checkout_sizes);
        let probe = plakous::tools::runner::workspace_probe();
        let sizes = tokio::task::spawn_blocking(move || {
            let mut cache = cache.lock().map_err(|_| Self::lock_poisoned())?;
            let sizes =
                probe_checkout_sizes(&mut cache, inventory, now, |path| probe.tree_bytes(path))
                    .map_err(|error| {
                        TopologyError::new(
                            TopologyErrorCode::MintUnavailable,
                            format!("copy sizes could not be probed: {error}"),
                        )
                    })?;
            Ok::<_, TopologyError>(sizes)
        })
        .await
        .map_err(|_| Self::lock_poisoned())??;
        // A release owner: the job stop and the dispatch it gates run to
        // their end, or to the deliberate abandon, whether the caller stays.
        let owner = self.clone();
        self.owned(async move { owner.reclaim_measured(now, sizes).await })
            .await
    }

    async fn reclaim_measured(
        &self,
        now: u64,
        sizes: std::collections::BTreeMap<String, u64>,
    ) -> Result<(), TopologyError> {
        let driver = self.clone();
        let (owners, effects) = self
            .mutate(move |tree| {
                let snapshot = tree.clone();
                match tree.reclaim_dormant(now, &sizes).and_then(|owners| {
                    if tree.revision != tree.stored_revision || !owners.is_empty() {
                        driver.save(tree)?;
                    }
                    Ok(owners)
                }) {
                    Ok(owners) => Ok(owners),
                    Err(error) => {
                        *tree = snapshot;
                        Err(error)
                    }
                }
            })
            .await?;
        let effects = self.hold_effects(effects);
        let owners = owners?;
        if let Err(error) = self.stop_agent_jobs(&owners).await {
            // No VCS effect was dispatched. Release only process-local
            // claims so the acknowledged intent survives for recovery.
            effects.abandon();
            let _ = self
                .mutate(ProductAgentTree::abandon_reclamation_dispatch)
                .await;
            return Err(error);
        }
        let settled = effects.settle().await?;
        if let Some(error) = settled.cleanup_failures.into_iter().next() {
            return Err(error);
        }
        Ok(())
    }

    /// Retires sealed dormant conversations after publishing exact deletion intent.
    /// Unadopted nonempty work refs are retained independently of runtime sessions.
    pub async fn collect_dormant_children(
        &self,
        sessions: &plakous::sessions::AgentSessions,
    ) -> Result<(), TopologyError> {
        let driver = self.clone();
        let (tickets, _) = self
            .mutate(move |tree| {
                let snapshot = tree.clone();
                match tree
                    .prepare_child_retention(plakous::model::now_unix_millis())
                    .and_then(|tickets| driver.save(tree).map(|()| tickets))
                {
                    Ok(tickets) => Ok(tickets),
                    Err(error) => {
                        *tree = snapshot;
                        Err(error)
                    }
                }
            })
            .await?;
        for ticket in tickets? {
            self.stop_agent_jobs(std::slice::from_ref(&ticket.agent_id))
                .await?;
            if let Some(raw) = &ticket.locator {
                let locator: plakous::sessions::SessionLocator = raw.parse().map_err(|_| {
                    TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "retired child locator is invalid; session retained",
                    )
                })?;
                match sessions.delete_archived(locator.session_id()).await {
                    Ok(_) => {}
                    Err(error)
                        if error.kind()
                            == plakous::sessions::AgentSessionsErrorKind::InvalidInput =>
                    {
                        let archived = sessions.archive(locator).await.map_err(|_| TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired, "child session retirement refused; exact session and work refs retained"))?;
                        sessions
                            .delete(archived.session_id(), archived.archived_revision())
                            .await
                            .map_err(|_| {
                                TopologyError::new(
                                    TopologyErrorCode::OperationRecoveryRequired,
                                    "child session deletion remains pending; work refs retained",
                                )
                            })?;
                    }
                    Err(_) => {
                        return Err(TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired,
                            "child session deletion remains pending; work refs retained",
                        ));
                    }
                }
            }
            let driver = self.clone();
            let (finished, _) = self
                .mutate(move |tree| {
                    driver.fold_and_commit(tree, |tree| tree.finish_child_retention(&ticket))
                })
                .await?;
            finished?;
        }
        self.cleanup_candidate_pins().await
    }

    async fn cleanup_candidate_pins(&self) -> Result<(), TopologyError> {
        let pending = self.read(ProductAgentTree::pending_pin_cleanup)??;
        let (namespace, launch, vcs) = self.read(|tree| {
            (
                plakous::model::sha256_hex(&[tree.tree_id.as_bytes()]),
                tree.launch_checkout.clone(),
                tree.vcs_port(),
            )
        })?;
        for ticket in pending {
            let unpinned = {
                let (vcs, launch, namespace, candidates) = (
                    vcs.clone(),
                    launch.clone(),
                    namespace.clone(),
                    ticket.candidates.clone(),
                );
                async move {
                    for candidate in &candidates {
                        vcs.unpin(&launch, &namespace, candidate.as_str()).await?;
                    }
                    Ok::<(), VcsError>(())
                }
                .await
            };
            if unpinned.is_err() {
                return Err(TopologyError::new(
                    TopologyErrorCode::WorkspaceRecoveryRequired,
                    "collected candidate pin cleanup remains pending; intent retained",
                ));
            }
            let driver = self.clone();
            let (finished, _) = self
                .mutate(move |tree| {
                    driver.fold_and_commit(tree, |tree| tree.acknowledge_pin_cleanup(&ticket))
                })
                .await?;
            finished?;
        }
        Ok(())
    }

    pub(crate) fn set_retirement_recovery(&self, recovery: RetirementRecovery) {
        *self
            .retirement_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(recovery);
    }

    pub(crate) fn observer(&self) -> Option<&Arc<dyn TopologyObserver>> {
        self.observer.as_ref()
    }

    pub(super) fn spawn_retirement(&self, work: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self
            .retirement_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.retain(|task| !task.is_finished());
        tasks.push(self.release_owners.spawn(work));
    }

    /// The tree's own task set, for work that owns no child session:
    /// projections, join observers, recovery passes. No root's release
    /// touches it; the shutdown of the tree's last root lease closes it.
    pub fn tasks(&self) -> crate::task_set::TaskSet {
        self.tasks.clone()
    }

    /// Spawns work that owns a child's session or runtime authority until it
    /// returns: startup and resume owners and their cleanups, turn waiters,
    /// follow-up deliveries, retirement release workers. No root's release
    /// and no tree shutdown aborts it. Shutdown bounds its join only on
    /// retirement completions and on cancelled startups, and leaves every
    /// other owner running detached. Process-exit abandonment aborts
    /// retirement workers; the other owners end with the process's executor
    /// (spec 03 section 16).
    pub fn spawn_release_owner<F>(&self, future: F) -> crate::task_set::TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.release_owners.spawn(future)
    }

    /// The set [`Self::spawn_release_owner`] spawns on.
    pub fn release_owners(&self) -> crate::task_set::TaskSet {
        self.release_owners.clone()
    }

    /// Joins tool-side receipt, job-stop and effect settlement after child release.
    /// Cancellation of the join retains the independently owned tasks.
    ///
    /// # Errors
    /// Reports a panicked or aborted retirement completion task.
    pub async fn join_retirement_tasks(&self) -> Result<(), String> {
        let mut failed = false;
        loop {
            // Each poll joins in place, so a cancelled join leaves every
            // unjoined task tracked for the next one.
            let next = std::future::poll_fn(|cx| {
                let mut tasks = self
                    .retirement_tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if tasks.is_empty() {
                    return std::task::Poll::Ready(None);
                }
                for index in 0..tasks.len() {
                    if let std::task::Poll::Ready(joined) = Pin::new(&mut tasks[index]).poll(cx) {
                        tasks.swap_remove(index);
                        return std::task::Poll::Ready(Some(joined));
                    }
                }
                std::task::Poll::Pending
            })
            .await;
            match next {
                None if failed => return Err("retirement completion task failed".into()),
                None => return Ok(()),
                Some(Ok(())) => {}
                Some(Err(_)) => failed = true,
            }
        }
    }

    /// Requests a pass after caller-layer retirement evidence is published.
    pub fn request_retirement_recovery(&self) {
        let recovery = self
            .retirement_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(recovery) = recovery {
            recovery();
        }
    }

    pub(super) fn bind_job_host(&self, host: Option<Arc<dyn plakous::tools::JobHost>>) {
        if let Some(host) = host {
            *self
                .job_host
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(host);
        }
    }

    pub(super) fn job_service(&self, agent: &str) -> Option<plakous::tools::JoinedSessionsPort> {
        let locator = self
            .read(|tree| tree.node(agent).ok()?.session_locator.clone())
            .ok()??;
        let locator: plakous::sessions::SessionLocator = locator.parse().ok()?;
        self.job_host
            .lock()
            .ok()?
            .as_ref()?
            .service_at(locator.session_id(), locator.branch_id())
    }

    /// Terminal-change edges of one agent's own process service, so a host
    /// that parks the agent wakes on a process exit without polling.
    #[must_use]
    pub fn job_terminal_changes(&self, agent: &str) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.job_service(agent)
            .map(|service| service.job_terminal_changes())
    }

    /// Stops promoted processes before an attempt's terminal seal.
    ///
    /// # Errors
    /// Refuses if exact process cleanup cannot be established.
    pub async fn stop_agent_jobs(&self, ids: &[String]) -> Result<(), TopologyError> {
        let expected = self.read(|tree| {
            ids.iter()
                .map(|id| {
                    (
                        id.clone(),
                        tree.attempt(id).map(|attempt| attempt.attempt_id.clone()),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        })?;
        let mut stops = tokio::task::JoinSet::new();
        for id in ids {
            if let Some(service) = self.job_service(id) {
                let id = id.clone();
                stops.spawn_blocking(move || {
                    let result = service.stop_jobs(
                        plakous::tools::JobStopReason::AttemptEnded,
                        std::time::Duration::from_millis(250),
                    );
                    (id, result)
                });
            }
        }
        let mut failures = Vec::new();
        while let Some(result) = stops.join_next().await {
            match result {
                Ok((_, Ok(()))) => {}
                Ok((id, Err(error))) => failures.push((id, error.to_string())),
                Err(error) => failures.extend(
                    ids.iter()
                        .map(|id| (id.clone(), format!("job stop worker failed: {error}"))),
                ),
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        let message = failures
            .iter()
            .map(|(id, error)| format!("{id}: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        let driver = self.clone();
        let recorded = self
            .write(move |tree| {
                let mut recorded = false;
                for (id, error) in failures {
                    recorded |= tree
                        .record_host_retirement_for_attempt(
                            &id,
                            expected.get(&id).and_then(Option::as_ref),
                            error,
                        )
                        .and_then(|value| {
                            driver.apply_recorded_guarded(tree, &id).map(|()| value)
                        })?;
                }
                driver.save_guarded(tree)?;
                Ok::<_, TopologyError>(recorded)
            })
            .await??;
        if recorded {
            self.request_retirement_recovery();
        }
        Err(TopologyError::new(
            TopologyErrorCode::OperationRecoveryRequired,
            message,
        ))
    }

    fn lock_poisoned() -> TopologyError {
        TopologyError::new(
            TopologyErrorCode::OperationRecoveryRequired,
            "topology tree lock is poisoned",
        )
    }

    /// Runs `work` as this tree's next writer, after every writer that asked
    /// before it, on a blocking worker with the live tree as its guard. The
    /// guard detaches lazily, on its first capacity-refused save.
    ///
    /// While queued, a writer gives up only when the tree cannot publish and
    /// its agent was cancelled or the tree or its host is shutting down: it
    /// would park at once. A capacity-refused save inside `work` parks here,
    /// in the caller, raced against the same signals; dropping this future
    /// stops the park, but `work` itself runs to its end and keeps the tree
    /// until then.
    ///
    /// # Errors
    ///
    /// Refuses when the tree is poisoned, or when a queued writer gives up.
    pub(crate) async fn write<R, W>(&self, work: W) -> Result<R, TopologyError>
    where
        W: FnOnce(&mut storage::TreeMutationGuard<'_>) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.write_turn(work, |_, _| {}, Turn::Abandonable).await
    }

    /// [`Self::write`] for work that is owed: a process-local release or an
    /// effect's completion. It waits for its turn whatever was cancelled, so
    /// what it releases or records is never stranded; only a publication
    /// inside it can be refused.
    pub(crate) async fn write_owed<R, W>(&self, work: W) -> Result<R, TopologyError>
    where
        W: FnOnce(&mut storage::TreeMutationGuard<'_>) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.write_turn(work, |_, _| {}, Turn::Owed).await
    }

    /// [`Self::write`], for work whose result claims process-local state the
    /// caller must release: when the caller is gone by the time `work`
    /// returns, `undo` gets the result in the same writer turn instead, and
    /// a result the caller dropped unread gets it as the next writer.
    pub(crate) async fn write_or_undo<R, W, U>(&self, work: W, undo: U) -> Result<R, TopologyError>
    where
        W: FnOnce(&mut storage::TreeMutationGuard<'_>) -> R + Send + 'static,
        U: Fn(&mut storage::TreeMutationGuard<'_>, R) + Send + Sync + 'static,
        R: Send + 'static,
    {
        self.write_turn(work, undo, Turn::Abandonable).await
    }

    /// The typed face of [`Self::write_erased`]: only the work, its result
    /// hand-off and its undo are generic, so each caller's closure does not
    /// instantiate the writer's queue, park loop and blocking task again.
    async fn write_turn<R, W, U>(&self, work: W, undo: U, turn: Turn) -> Result<R, TopologyError>
    where
        W: FnOnce(&mut storage::TreeMutationGuard<'_>) -> R + Send + 'static,
        U: Fn(&mut storage::TreeMutationGuard<'_>, R) + Send + Sync + 'static,
        R: Send + 'static,
    {
        let undo = Arc::new(undo);
        let (deliver, delivered) = tokio::sync::oneshot::channel();
        let mut delivered = Unread {
            result: Some(delivered),
            on_unread: Some({
                let (driver, undo) = (self.clone(), Arc::clone(&undo));
                Box::new(move |result: R| {
                    driver.undo_as_next_writer(Box::new(move |tree| (*undo)(tree, result)));
                })
            }),
        };
        let work: ErasedWork = Box::new(move |tree| {
            let result = work(tree);
            if let Err(result) = deliver.send(result) {
                (*undo)(tree, result);
            }
        });
        self.write_erased(work, turn).await?;
        delivered.take().await.map_err(|_| Self::lock_poisoned())
    }

    /// Runs `undo` as the tree's next writer, on a blocking worker: an undo
    /// never saves, so it needs no park.
    fn undo_as_next_writer(&self, undo: ErasedWork) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let driver = self.clone();
        drop(tokio::task::spawn_blocking(move || {
            let _permit = driver.writer.order().blocking_lock_owned();
            if let Ok(mut tree) =
                storage::TreeMutationGuard::lock(&driver.tree, &driver.restored_paused)
            {
                undo(&mut tree);
            }
        }));
    }

    /// Waits for the writer's turn as `turn` says, then runs `work` on a
    /// blocking worker, waiting out any capacity park it hands back here.
    async fn write_erased(&self, work: ErasedWork, turn: Turn) -> Result<(), TopologyError> {
        if self.writer.is_poisoned() {
            return Err(Self::lock_poisoned());
        }
        // The host this request was made for, as the tree knows it now.
        let host = self.host_shutdown_signal();
        let queued = self.storage_parking.begin(self.publication_agent());
        let permit = match turn {
            Turn::Owed => self.writer.order().lock_owned().await,
            Turn::Abandonable => self.queue_writer(queued.token(), host.as_ref()).await?,
        };
        drop(queued);
        let (parks, mut parked) = tokio::sync::mpsc::unbounded_channel();
        let driver = self.clone();
        let mut running = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _writer = driver.writer.enter(parks);
            let mut tree = storage::TreeMutationGuard::lock(&driver.tree, &driver.restored_paused)
                .map_err(|()| Self::lock_poisoned())?;
            work(&mut tree);
            Ok::<_, TopologyError>(())
        });
        loop {
            let finished = match select(&mut running, Box::pin(parked.recv())).await {
                Either::Left((finished, _)) => finished,
                Either::Right((Some(park), _)) => {
                    let retry = self.park(&park.token, host.as_ref()).await;
                    let _ = park.resume.send(retry);
                    continue;
                }
                Either::Right((None, running)) => running.await,
            };
            return match finished {
                Ok(result) => result,
                Err(error) if error.is_panic() => {
                    self.writer.poison();
                    Err(Self::lock_poisoned())
                }
                // The runtime is shutting down under the writer: nothing ran
                // to its end that a retry could not run again.
                Err(_) => Err(TopologyError::new(
                    TopologyErrorCode::RecoveryStorageUnavailable,
                    "the tree writer was cancelled with its runtime",
                )),
            };
        }
    }

    fn publication_agent(&self) -> &str {
        self.publication_owner
            .as_deref()
            .unwrap_or(&self.root_agent_id)
    }

    /// Waits for this writer's turn, giving up only once the tree cannot
    /// publish while the agent, the tree or the host is stopping.
    async fn queue_writer(
        &self,
        cancelled: &tokio_util::sync::CancellationToken,
        host: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, TopologyError> {
        let order = self.writer.order();
        let mut turn = Box::pin(order.lock_owned());
        let mut stopping = Box::pin(self.stopping(cancelled, host));
        let mut unavailable = self.storage_waiting.subscribe();
        // Before the stop is signalled, only it or the turn can end the wait.
        match select(&mut turn, &mut stopping).await {
            Either::Left((permit, _)) => return Ok(permit),
            Either::Right(((), _)) => {}
        }
        loop {
            if *unavailable.borrow_and_update() {
                return Err(TopologyError::new(
                    TopologyErrorCode::RecoveryStorageUnavailable,
                    "tree publication stopped while queued behind a storage outage",
                ));
            }
            match select(&mut turn, Box::pin(unavailable.changed())).await {
                Either::Left((permit, _)) => return Ok(permit),
                Either::Right((Ok(()), _)) => {}
                Either::Right((Err(_), _)) => return Ok(turn.await),
            }
        }
    }

    /// One capacity park: `true` to retry the save after the interval,
    /// `false` once the write's agent was cancelled or the tree or its host
    /// is shutting down.
    async fn park(
        &self,
        cancelled: &tokio_util::sync::CancellationToken,
        host: Option<&tokio_util::sync::CancellationToken>,
    ) -> bool {
        if cancelled.is_cancelled()
            || self.storage_parking.stopped().is_cancelled()
            || host.is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return false;
        }
        let interval = Box::pin(tokio::time::sleep(self.capacity_retry_interval));
        matches!(
            select(interval, Box::pin(self.stopping(cancelled, host))).await,
            Either::Left(_)
        )
    }

    /// Resolves once the write's agent is cancelled or the tree or its host
    /// is shutting down: latched signals, so it never polls.
    async fn stopping(
        &self,
        cancelled: &tokio_util::sync::CancellationToken,
        host: Option<&tokio_util::sync::CancellationToken>,
    ) {
        let host = async {
            match host {
                Some(host) => host.cancelled().await,
                None => std::future::pending().await,
            }
        };
        let tree = self.storage_parking.stopped().cancelled();
        let _ = select(
            Box::pin(cancelled.cancelled()),
            select(Box::pin(tree), Box::pin(host)),
        )
        .await;
    }

    pub(crate) fn is_restored_paused(&self) -> bool {
        self.restored_paused
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Binds subsequent publication waits to the agent that owns this work.
    #[must_use]
    pub fn for_agent(&self, agent_id: &str) -> Self {
        Self {
            publication_owner: Some(agent_id.to_owned()),
            ..self.clone()
        }
    }

    /// Reads the tree under its mutex.
    ///
    /// `work` gets `&ProductAgentTree`, so a read cannot queue an effect and
    /// therefore cannot strand one.
    ///
    /// # Errors
    ///
    /// Refuses when the tree mutex is poisoned.
    pub fn read<R>(&self, work: impl FnOnce(&ProductAgentTree) -> R) -> Result<R, TopologyError> {
        if self.writer.is_poisoned() {
            return Err(Self::lock_poisoned());
        }
        let tree = self.tree.lock().map_err(|_| Self::lock_poisoned())?;
        Ok(work(&tree))
    }

    /// Reads the tree without waiting for its mutex.
    ///
    /// `None` when the tree is locked or poisoned — a caller rendering a view
    /// would rather skip a frame than block behind a turn.
    pub fn try_read<R>(&self, work: impl FnOnce(&ProductAgentTree) -> R) -> Option<R> {
        if self.writer.is_poisoned() {
            return None;
        }
        let tree = self.tree.try_lock().ok()?;
        Some(work(&tree))
    }

    /// Raw tree access for fixtures that install or inspect a synthetic tree.
    /// Production consumers use closure mutations and settle their effects.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn tree_handle_for_test(&self) -> &Arc<Mutex<ProductAgentTree>> {
        &self.tree
    }

    /// Runs `work` as the tree's next writer and takes the effects it queued.
    ///
    /// Clones the whole tree on every call so a callback that publishes can
    /// park without holding the live tree mutex. Internal guarded mutations
    /// instead detach lazily on their first capacity-refused save. `work` is
    /// a message: it owns what it uses and runs on a blocking worker.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned tree, or a queued writer that gave up.
    pub async fn mutate<R, W>(&self, work: W) -> Result<(R, Effects), TopologyError>
    where
        W: FnOnce(&mut ProductAgentTree) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.mutate_turn(work, Turn::Abandonable).await
    }

    /// [`Self::mutate`] for owed work, such as releasing a process-local
    /// registration: it waits for its turn whatever was cancelled.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned tree.
    pub async fn mutate_owed<R, W>(&self, work: W) -> Result<(R, Effects), TopologyError>
    where
        W: FnOnce(&mut ProductAgentTree) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.mutate_turn(work, Turn::Owed).await
    }

    async fn mutate_turn<R, W>(&self, work: W, turn: Turn) -> Result<(R, Effects), TopologyError>
    where
        W: FnOnce(&mut ProductAgentTree) -> R + Send + 'static,
        R: Send + 'static,
    {
        let owner = self.clone();
        self.write_turn(
            move |tree| {
                // The callback receives a plain prepared tree and can publish it.
                // Unlike the internal guarded path, its borrow cannot release a lock.
                tree.detach();
                let result = work(tree);
                let effects = tree.take_effects();
                (result, effects)
            },
            // A caller gone before its effects reached it leaves them to a
            // release owner, which settles them.
            move |_, (_, effects): (R, Effects)| owner.settle_detached(effects),
            turn,
        )
        .await
    }

    /// Settles `effects` no caller is left to settle, as a release owner.
    pub(crate) fn settle_detached(&self, effects: Effects) {
        if effects.is_empty() {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            // Nothing can run them here; their tickets reopen as recovery.
            tracing::error!(target: "plak.topology",
                "effects of a departed caller could not be settled: no runtime");
            return;
        }
        let driver = self.clone();
        drop(self.spawn_release_owner(async move {
            if let Err(error) = driver.settle_here(effects).await {
                tracing::error!(target: "plak.topology", code = ?error.code,
                    "effects settled for a departed caller were refused");
            }
        }));
    }

    /// Holds `effects` for a caller that awaits other work before settling
    /// them: dropped unsettled, by an early return or an abort, they are
    /// settled by a release owner.
    pub fn hold_effects(&self, effects: Effects) -> HeldEffects {
        HeldEffects {
            driver: self.clone(),
            effects: Some(effects),
        }
    }

    /// One mutation, settled: applies `work`, then runs everything it queued
    /// with the mutex released and records the result.
    ///
    /// The pairing a caller above this crate should reach for, because the
    /// two halves cannot come apart — [`Self::mutate`] hands the effects out
    /// and only a caller that then drops them can strand one.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned mutex, and reports the first save error the drain
    /// hit after every effect the port ran has been recorded.
    pub async fn mutate_and_settle<R, W>(&self, work: W) -> Result<(R, Settled), TopologyError>
    where
        W: FnOnce(&mut ProductAgentTree) -> R + Send + 'static,
        R: Send + 'static,
    {
        // A release owner: a caller that leaves mid-way strands no effect.
        let driver = self.clone();
        self.owned(async move { driver.mutate_and_settle_here(work).await })
            .await
    }

    async fn mutate_and_settle_here<R, W>(&self, work: W) -> Result<(R, Settled), TopologyError>
    where
        W: FnOnce(&mut ProductAgentTree) -> R + Send + 'static,
        R: Send + 'static,
    {
        let driver = self.clone();
        let (result, effects) = self
            .mutate(move |tree| {
                let result = work(tree);
                driver.save(tree).map(|()| result)
            })
            .await?;
        let result = result?;
        let settled = self.settle_here(effects).await?;
        Ok((result, settled))
    }

    /// Runs `effects` against the port with the mutex released, then
    /// re-locks once to record them and publishes the tree. Recording can
    /// owe more, so an acknowledged result save continues the drain. A
    /// refused result save retains follow-on effects without dispatching
    /// them. Every effect already run is recorded before reporting the error.
    /// Destructive removal requires an acknowledged preparation; a refused
    /// preparation retains the copy and records recovery without dispatch.
    ///
    /// The port's answers are awaited outside every lock; each recording is
    /// one writer.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned tree, effects without an acknowledged revision,
    /// and reports the first save error after recording every run effect.
    pub async fn settle(&self, effects: Effects) -> Result<Settled, TopologyError> {
        if effects.is_empty() {
            return Ok(Settled::default());
        }
        // A release owner: a caller that leaves mid-way strands no effect.
        let driver = self.clone();
        self.owned(async move { driver.settle_here(effects).await })
            .await
    }

    /// Runs `work` as a release owner and waits for it: a caller that leaves
    /// leaves it running.
    pub(crate) async fn owned<T, F>(&self, work: F) -> Result<T, TopologyError>
    where
        F: Future<Output = Result<T, TopologyError>> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_release_owner(work).await.map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "the tree's release owner failed",
            )
        })?
    }

    /// [`Self::settle`] on the calling task, for a caller that already is
    /// a release owner.
    pub(crate) async fn settle_here(&self, mut effects: Effects) -> Result<Settled, TopologyError> {
        let mut settled = Settled::default();
        let mut first_save_error = None;
        while !effects.is_empty() {
            let (admitted, refused) = self
                .write_owed(move |tree| {
                    let refused = Self::admit_settled_effects(tree, &mut effects)?;
                    Ok::<_, TopologyError>((effects, refused))
                })
                .await??;
            let vcs = Arc::clone(&self.vcs);
            // The admitted batch is a task of its own, awaited here: once
            // dispatched it runs every port call to its end even if this
            // release owner is aborted. It ends with the Tokio runtime
            // (spec 03 §16), between two port calls.
            let mut completed = tokio::spawn(async move { admitted.run(vcs.as_ref()).await })
                .await
                .map_err(|_| {
                    TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "effects task failed",
                    )
                })?;
            completed.refuse_unpublished_unmints(refused);
            let driver = self.clone();
            // Recorded whenever the turn comes: only the publication below
            // may be refused.
            let (more, next, save_error) = self
                .write_owed(move |tree| {
                    let more = tree.complete(completed);
                    tree.lift_restored_pause_if_quiescent();
                    if let Err(error) = driver.save_guarded(tree) {
                        // No follow-on refresh write was dispatched. Keep its intent
                        // for exact-state recovery rather than a stranded local hold.
                        tree.abandon_queued_refresh_dispatches();
                        // Completed effects remain recorded. Follow-on reservations
                        // stay queued until a later successful publication.
                        return (more, Effects::default(), Some(error));
                    }
                    (more, tree.take_effects(), None)
                })
                .await?;
            settled.adopts.extend(more.adopts);
            settled.hook_failures.extend(more.hook_failures);
            settled.seal_failures.extend(more.seal_failures);
            settled.cleanup_failures.extend(more.cleanup_failures);
            if let Some(error) = save_error {
                first_save_error = Some(error);
                break;
            }
            effects = next;
        }
        Self::settled_outcome(settled, first_save_error)
    }

    /// Refuses effects that need an acknowledged revision on an
    /// unacknowledged tree, and takes the unmints the tree no longer admits.
    fn admit_settled_effects(
        tree: &ProductAgentTree,
        effects: &mut Effects,
    ) -> Result<Vec<crate::tree::UnmintTicket>, TopologyError> {
        if tree.revision != tree.stored_revision
            && (!effects.base_pins.is_empty()
                || !effects.refreshes.is_empty()
                || !effects.seals.is_empty()
                || !effects.adopts.is_empty()
                || !effects.dormancy.is_empty())
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "seal and adopt effects require an acknowledged revision",
            ));
        }
        let mut refused = Vec::new();
        effects.unmints.retain(|ticket| {
            let admitted = tree.revision == tree.stored_revision
                && ticket.workspace_id.as_deref().is_some_and(|id| {
                    tree.workspace(id).is_some_and(|row| {
                        row.status == crate::protocol::WorkspaceStatus::Discarding
                            && row.path == ticket.path
                    })
                })
                && ticket.operation_id.as_deref().is_none_or(|id| {
                    tree.operations().get(id).is_some_and(|receipt| {
                        receipt.terminal_response.is_none() && receipt.terminal_code.is_none()
                    })
                });
            if !admitted {
                refused.push(ticket.clone());
            }
            admitted
        });
        Ok(refused)
    }

    fn settled_outcome(
        settled: Settled,
        first_save_error: Option<TopologyError>,
    ) -> Result<Settled, TopologyError> {
        match first_save_error {
            Some(error) => {
                if let Some(seal) = settled.seal_failures.first() {
                    // Leave wire space for both diagnostics and the host's recovery prefix.
                    Err(TopologyError::new(
                        error.code,
                        format!(
                            "{}; seal failure: {}",
                            bounded_text(&error.message, 128),
                            bounded_text(&seal.message, 128),
                        ),
                    ))
                } else {
                    Err(error)
                }
            }
            None => {
                if let Some(error) = settled.cleanup_failures.first() {
                    Err(error.clone())
                } else {
                    Ok(settled)
                }
            }
        }
    }

    /// The port the runtime runs a tree's tickets on, shared. Used for the
    /// mint a `spawn_reserve` owes, which runs with the mutex released.
    pub fn vcs(&self) -> Arc<dyn VcsOps> {
        Arc::clone(&self.vcs)
    }

    pub(crate) async fn fold_startup(&self) -> Result<Effects, TopologyError> {
        let driver = self.clone();
        self.write(move |tree| {
            if tree.revision != tree.stored_revision {
                driver.save_guarded(tree)?;
            }
            let targets = tree.startup_fold_targets();
            let lift_pause = tree.state == crate::protocol::TreeAdmissionState::RestoredPaused
                && tree.quiescence_refusal().is_none();
            if targets.is_empty() && tree.pending_close_folds.is_empty() && !lift_pause {
                return Ok(tree.take_effects());
            }
            driver.fold_and_commit_guarded(tree, |tree| {
                for agent in targets {
                    tree.fold_due_in_place(&agent)?;
                }
                Ok(())
            })?;
            Ok(tree.take_effects())
        })
        .await?
    }

    /// Whole-tree publish as the store's compare-and-swap:
    /// refused with `StaleRevision` when another handle published this tree
    /// since it was loaded. Always sent — an operation receipt lands
    /// without a revision bump — and always safe: an unchanged revision is
    /// a compare-and-swap hit only for the handle the store acknowledged.
    ///
    /// Call it inside [`Self::mutate`]: a capacity refusal parks only in a
    /// writer; anywhere else it is returned at once.
    pub fn save(&self, tree: &mut ProductAgentTree) -> Result<(), TopologyError> {
        let owner = self
            .publication_owner
            .as_deref()
            .unwrap_or(&tree.root_agent_id);
        let wait = self.storage_parking.begin(owner);
        self.save_prepared(tree, None, &wait)
    }

    pub(crate) fn save_guarded(
        &self,
        tree: &mut storage::TreeMutationGuard<'_>,
    ) -> Result<(), TopologyError> {
        let owner = self
            .publication_owner
            .as_deref()
            .unwrap_or(&tree.root_agent_id);
        let wait = self.storage_parking.begin(owner);
        let first = self.save_claimed(tree);
        if matches!(&first, Err(error) if error.kind == crate::tree::TreeStoreErrorKind::StorageFull)
        {
            tree.detach();
        }
        self.save_prepared(tree, Some(first), &wait)
    }

    fn save_prepared(
        &self,
        tree: &mut ProductAgentTree,
        mut first: Option<Result<crate::tree::Revision, crate::tree::TreeStoreError>>,
        wait: &storage::StorageWait,
    ) -> Result<(), TopologyError> {
        let acknowledged = loop {
            match first.take().unwrap_or_else(|| self.save_claimed(tree)) {
                Ok(revision) => {
                    self.storage_waiting
                        .send_if_modified(|waiting| std::mem::replace(waiting, false));
                    break revision;
                }
                Err(error) if error.kind == crate::tree::TreeStoreErrorKind::StorageFull => {
                    self.storage_waiting
                        .send_if_modified(|waiting| !std::mem::replace(waiting, true));
                    if !self.writer.park(wait.token()) {
                        return Err(error.into());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        };
        tree.acknowledge_revision(acknowledged);
        for terminal in std::mem::take(&mut tree.pending_turn_usages).into_values() {
            if let Some(observer) = &self.observer {
                observer.turn_usage(terminal);
            }
        }
        Ok(())
    }

    /// Process-local publication pause, readable without taking the tree lock.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn storage_unavailable(&self) -> bool {
        *self.storage_waiting.borrow()
    }

    /// Interrupts a capacity-refused publication, including a write whose
    /// refusal has not returned yet, without claiming it was recorded.
    pub fn cancel_storage_waits(&self, agent_id: &str) {
        self.storage_parking.cancel(agent_id);
    }

    pub(crate) fn shutdown_storage_waits(&self) {
        self.storage_parking.shutdown();
    }

    /// Applies consequences only to an acknowledged tree, and publishes them
    /// before their effects may be drained. A refusal restores the exact
    /// pre-work snapshot, including the feed cursor and pending usage.
    ///
    /// # Errors
    /// Refuses unacknowledged input, a failed mutation, or a refused save.
    /// `work` must not save: intent publication precedes this boundary.
    pub fn fold_and_commit<R>(
        &self,
        tree: &mut ProductAgentTree,
        work: impl FnOnce(&mut ProductAgentTree) -> Result<R, TopologyError>,
    ) -> Result<R, TopologyError> {
        if tree.revision != tree.stored_revision {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "tree consequences require an acknowledged revision",
            ));
        }
        let snapshot = tree.clone();
        match Self::fold_work(tree, work).and_then(|result| self.save(tree).map(|()| result)) {
            Ok(result) => Ok(result),
            Err(error) => {
                *tree = snapshot;
                Err(error)
            }
        }
    }

    pub(crate) fn fold_and_commit_guarded<R>(
        &self,
        tree: &mut storage::TreeMutationGuard<'_>,
        work: impl FnOnce(&mut ProductAgentTree) -> Result<R, TopologyError>,
    ) -> Result<R, TopologyError> {
        if tree.revision != tree.stored_revision {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "tree consequences require an acknowledged revision",
            ));
        }
        let snapshot = (**tree).clone();
        match Self::fold_work(tree, work)
            .and_then(|result| self.save_guarded(tree).map(|()| result))
        {
            Ok(result) => Ok(result),
            Err(error) => {
                **tree = snapshot;
                Err(error)
            }
        }
    }

    fn fold_work<R>(
        tree: &mut ProductAgentTree,
        work: impl FnOnce(&mut ProductAgentTree) -> Result<R, TopologyError>,
    ) -> Result<R, TopologyError> {
        let result = work(tree)?;
        if tree.state == crate::protocol::TreeAdmissionState::RestoredPaused {
            for agent in tree.startup_fold_targets() {
                tree.fold_due_in_place(&agent)?;
            }
        }
        tree.fold_reserved_closes_in_place()?;
        tree.refresh_epoch_after_close();
        tree.prune_closed_copies();
        tree.lift_restored_pause_if_quiescent();
        Ok(result)
    }

    /// Withdraws an unfacted followup in one revision CAS. The caller holds
    /// the child session and startup reservation while reading `tail` and
    /// calling this method; a failed guard or save changes no live rows.
    /// The opening envelope and optional send failure receipt publish with it.
    ///
    /// # Errors
    /// Refuses a changed feed, another intent/effect, or a failed revision CAS.
    pub async fn withdraw_unstarted_followup(
        &self,
        agent_id: &str,
        message_id: &str,
        session_id: &str,
        tail: u64,
        receipt: Option<(String, TopologyErrorCode)>,
    ) -> Result<(), TopologyError> {
        let driver = self.clone();
        let (agent_id, message_id, session_id) = (
            agent_id.to_owned(),
            message_id.to_owned(),
            session_id.to_owned(),
        );
        self.write(move |tree| {
            driver.fold_and_commit_guarded(tree, |tree| {
                tree.withdraw_unstarted_followup(&agent_id, &message_id, &session_id, tail)?;
                settle_followup_refusal(tree, &agent_id, &message_id, receipt)
            })
        })
        .await
        .map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "tree lock is poisoned",
            )
        })?
    }

    /// Publishes recorded host decisions before projecting their consequences.
    ///
    /// # Errors
    /// A refused intent save leaves the views and effect reservations untouched.
    /// Reports a malformed intent or a refused consequence save.
    pub fn apply_recorded(
        &self,
        tree: &mut ProductAgentTree,
        agent_id: &str,
    ) -> Result<(), TopologyError> {
        let driver = self.for_agent(agent_id);
        driver.save_host_intent(tree, agent_id)?;
        driver.fold_and_commit(tree, |tree| {
            tree.fold_due_in_place(agent_id)?;
            tree.lift_restored_pause_if_quiescent();
            Ok(())
        })
    }

    pub(crate) fn apply_recorded_guarded(
        &self,
        tree: &mut storage::TreeMutationGuard<'_>,
        agent_id: &str,
    ) -> Result<(), TopologyError> {
        let driver = self.for_agent(agent_id);
        driver.save_guarded(tree)?;
        driver.fold_and_commit_guarded(tree, |tree| {
            tree.fold_due_in_place(agent_id)?;
            tree.lift_restored_pause_if_quiescent();
            Ok(())
        })
    }

    fn save_host_intent(
        &self,
        tree: &mut ProductAgentTree,
        agent_id: &str,
    ) -> Result<(), TopologyError> {
        self.save(tree).inspect_err(|error| {
            if error.code == TopologyErrorCode::RecoveryStorageUnavailable {
                return;
            }
            let reason = tree.fault_reason(agent_id).map_or_else(
                || format!("tree publish refused: {}", error.message),
                |prior| format!("{prior}; tree publish refused: {}", error.message),
            );
            tree.record_fault(agent_id, reason);
        })
    }
}

/// The driver for a tree the process hub already owns, or — in tests only —
/// a fresh one over an in-memory store and the tree's own port. Production
/// tool sets and the turn waiter run on the hub's own driver
/// ([`topology_tool_registrations_on`], [`invoke_granted_topology_tool_on`]);
/// this is the bridge for the prompt goldens and unit tests that still hold
/// the bare tree `Arc`.
pub(super) fn driver_for_tree(
    tree: &Arc<Mutex<ProductAgentTree>>,
) -> TreeRuntimeResult<TreeDriver> {
    let (tree_id, vcs) = {
        let guard = tree
            .lock()
            .map_err(|_| TreeRuntimeError::new("topology tree lock is poisoned"))?;
        (guard.tree_id.clone(), guard.vcs_port())
    };
    if let Some(runtime) = crate::hub::lookup(&tree_id)
        && Arc::ptr_eq(&runtime.driver().tree, tree)
    {
        return Ok(runtime.driver().clone());
    }
    #[cfg(any(test, feature = "test-support"))]
    {
        Ok(TreeDriver::new(
            Arc::clone(tree),
            Arc::new(crate::tree::MemoryTreeStore::new()),
            vcs,
            None,
        ))
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        drop(vcs);
        Err(TreeRuntimeError::new(format!(
            "topology tree {tree_id} is not attached to the hub; no store to persist it"
        )))
    }
}

mod attach;
#[cfg(any(test, feature = "test-support"))]
mod test_support;

/// Effects a caller holds across other awaits; see
/// [`TreeDriver::hold_effects`].
#[must_use = "dropped unsettled, the effects go to a release owner"]
pub struct HeldEffects {
    driver: TreeDriver,
    effects: Option<Effects>,
}

impl HeldEffects {
    /// Settles the held effects as [`TreeDriver::settle`] does.
    ///
    /// # Errors
    ///
    /// As [`TreeDriver::settle`].
    pub async fn settle(mut self) -> Result<Settled, TopologyError> {
        let effects = self.effects.take().unwrap_or_default();
        self.driver.settle(effects).await
    }

    /// Gives the effects up unrun, for a caller that deliberately leaves
    /// their tickets to recovery.
    pub fn abandon(mut self) {
        drop(self.effects.take());
    }
}

impl Drop for HeldEffects {
    fn drop(&mut self) {
        if let Some(effects) = self.effects.take() {
            self.driver.settle_detached(effects);
        }
    }
}

impl std::fmt::Debug for HeldEffects {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HeldEffects")
            .field("effects", &self.effects)
            .finish_non_exhaustive()
    }
}

/// A writer's result on its way to its caller. Dropped unread, it hands the
/// result to the writer's undo as the tree's next writer.
struct Unread<R> {
    result: Option<tokio::sync::oneshot::Receiver<R>>,
    on_unread: Option<Box<dyn FnOnce(R) + Send>>,
}

impl<R> Unread<R> {
    fn take(&mut self) -> tokio::sync::oneshot::Receiver<R> {
        self.result
            .take()
            .unwrap_or_else(|| tokio::sync::oneshot::channel().1)
    }
}

impl<R> Drop for Unread<R> {
    fn drop(&mut self) {
        if let (Some(mut result), Some(on_unread)) = (self.result.take(), self.on_unread.take())
            && let Ok(result) = result.try_recv()
        {
            on_unread(result);
        }
    }
}

#[cfg(test)]
mod reclamation_probe_tests {
    use super::*;

    #[test]
    fn successful_trim_invalidates_size_inside_backoff_and_clears_budget_pressure() {
        let mut cache = std::collections::BTreeMap::new();
        let path = std::path::PathBuf::from("synthetic-copy");
        let mut walks = 0;
        let mut probe = |epoch, now, bytes| {
            probe_checkout_sizes(
                &mut cache,
                vec![("workspace".into(), path.clone(), 1, 3_600_000, epoch)],
                now,
                |_| {
                    walks += 1;
                    Ok(Some(bytes))
                },
            )
            .unwrap()
        };
        let before = probe(0, 0, 2048);
        assert!(before.values().sum::<u64>() > 1024);
        let after = probe(1, 1, 128);
        assert!(after.values().sum::<u64>() < 1024);
        assert_eq!(walks, 2);
    }

    #[test]
    fn size_walk_is_cached_through_backoff_but_invalidated_by_generation() {
        let mut cache = std::collections::BTreeMap::new();
        let path = std::path::PathBuf::from("synthetic-copy");
        let mut walks = 0;
        for (now, generation, retry_at) in [
            (0, 1, 0),
            (29_999, 1, 0),
            (30_000, 1, 90_000),
            (89_999, 1, 90_000),
            (90_000, 1, 90_000),
            (90_001, 2, 0),
        ] {
            let sizes = probe_checkout_sizes(
                &mut cache,
                vec![("workspace".into(), path.clone(), generation, retry_at, 0)],
                now,
                |_| {
                    walks += 1;
                    Ok(Some(123))
                },
            )
            .unwrap();
            assert_eq!(sizes.get("workspace"), Some(&123));
        }
        assert_eq!(walks, 3);
    }
}
