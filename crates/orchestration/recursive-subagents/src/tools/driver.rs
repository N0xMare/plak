//! The tree driver: one tree, its store and its VCS port, and the effects it settles.

use super::*;

/// One tree with its store and its VCS port: the engine every tree mutation
/// runs on. [`Self::mutate`] locks and applies; the effects the
/// mutation queued come out with the result and [`Self::settle`] runs them
/// with the mutex released, re-locking once to record them and publish.
/// [`Self::save`] is the fenced whole-tree publish. The guard a caller sees
/// inside `mutate` is the tree alone — it offers no VCS, so a subprocess
/// under the lock is not expressible here.
#[derive(Clone)]
pub struct TreeDriver {
    /// The tree under its one mutex.
    ///
    /// `pub(crate)` and not `pub`: the tree's own tools interleave locked and
    /// unlocked phases deliberately and are reviewed as such, but a caller
    /// above this crate that locked the tree could mutate it and drop the
    /// guard without ever taking the [`Effects`] that mutation queued — a
    /// stranded seal or adopt, which reopens `RecoveryRequired`. Outside
    /// callers get [`Self::read`] and [`Self::mutate`] instead, and the
    /// mutation path cannot lose its effects because it returns them.
    pub(crate) tree: Arc<Mutex<ProductAgentTree>>,
    pub(super) store: Arc<dyn TreeStore>,
    pub(super) vcs: Arc<dyn VcsOps>,
    pub(super) job_host: Arc<Mutex<Option<Arc<dyn plakous::tools::JobHost>>>>,
    retirement_recovery: Arc<Mutex<Option<RetirementRecovery>>>,
    pub(super) observer: Option<Arc<dyn TopologyObserver>>,
    next_reclaim_probe: Arc<Mutex<u64>>,
}

type RetirementRecovery = Arc<dyn Fn() + Send + Sync>;

fn directory_bytes(path: &std::path::Path) -> std::io::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut bytes = metadata.len();
    for entry in std::fs::read_dir(path)? {
        bytes = bytes.saturating_add(directory_bytes(&entry?.path())?);
    }
    Ok(bytes)
}

impl TreeDriver {
    /// Binds `tree` to `store` and `vcs`, installing `vcs` as the tree's own
    /// port so the mint inside `spawn_agent` and the effects run here drive
    /// one implementation.
    pub fn new(
        tree: Arc<Mutex<ProductAgentTree>>,
        store: Arc<dyn TreeStore>,
        vcs: Arc<dyn VcsOps>,
        observer: Option<Arc<dyn TopologyObserver>>,
    ) -> Self {
        if let Ok(mut guard) = tree.lock() {
            guard.set_vcs(Arc::clone(&vcs));
        }
        Self {
            tree,
            store,
            vcs,
            job_host: Arc::new(Mutex::new(None)),
            retirement_recovery: Arc::new(Mutex::new(None)),
            observer,
            next_reclaim_probe: Arc::new(Mutex::new(0)),
        }
    }

    /// Reclaims only acknowledged, settled dormant copies. Disk probes and
    /// VCS work run on blocking workers, never under the tree mutex.
    pub async fn reclaim_dormant_checkouts(&self) -> Result<(), TopologyError> {
        let now = plakous::model::now_unix_millis();
        let delay = self.read(|tree| tree.snapshot.workspace_idle_seconds)?;
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
        let inventory = self.read(ProductAgentTree::dormant_checkouts)?;
        let sizes = tokio::task::spawn_blocking(move || {
            inventory
                .into_iter()
                .filter_map(|(id, path)| directory_bytes(&path).ok().map(|bytes| (id, bytes)))
                .collect::<std::collections::BTreeMap<_, _>>()
        })
        .await
        .map_err(|_| Self::lock_poisoned())?;
        let (owners, effects) = self.mutate(|tree| {
            let snapshot = tree.clone();
            match tree
                .reclaim_dormant(now, &sizes)
                .and_then(|owners| self.save(tree).map(|()| owners))
            {
                Ok(owners) => Ok(owners),
                Err(error) => {
                    *tree = snapshot;
                    Err(error)
                }
            }
        })?;
        let owners = owners?;
        if let Err(error) = self.stop_agent_jobs(&owners).await {
            // No VCS effect was dispatched. Release only process-local
            // claims so the acknowledged intent survives for recovery.
            let _ = self.mutate(ProductAgentTree::abandon_reclamation_dispatch);
            return Err(error);
        }
        let driver = self.clone();
        let settled = tokio::task::spawn_blocking(move || driver.settle(effects))
            .await
            .map_err(|_| Self::lock_poisoned())??;
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
        let (tickets, _) = self.mutate(|tree| {
            let snapshot = tree.clone();
            match tree
                .prepare_child_retention(plakous::model::now_unix_millis())
                .and_then(|tickets| self.save(tree).map(|()| tickets))
            {
                Ok(tickets) => Ok(tickets),
                Err(error) => {
                    *tree = snapshot;
                    Err(error)
                }
            }
        })?;
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
            let namespace =
                self.read(|tree| plakous::model::sha256_hex(&[tree.tree_id.as_bytes()]))?;
            let launch = self.read(|tree| tree.launch_checkout.clone())?;
            let vcs = self.read(ProductAgentTree::vcs_port)?;
            for candidate in &ticket.pins {
                vcs.unpin(&launch, &namespace, candidate.as_str())
                    .map_err(|_| {
                        TopologyError::new(
                            TopologyErrorCode::WorkspaceRecoveryRequired,
                            "collected child pin cleanup remains pending; other work refs retained",
                        )
                    })?;
            }
            let (finished, _) = self.mutate(|tree| {
                self.fold_and_commit(tree, |tree| tree.finish_child_retention(&ticket))
            })?;
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

    pub(super) fn job_service(&self, agent: &str) -> Option<plakous::tools::JoinedProcessSessions> {
        let locator = self
            .read(|tree| tree.node(agent).ok()?.session_locator.clone())
            .ok()??;
        let locator: plakous::sessions::SessionLocator = locator.parse().ok()?;
        self.job_host
            .lock()
            .ok()?
            .as_ref()?
            .service(locator.session_id(), locator.branch_id())
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
        let mut tree = self.lock()?;
        let mut recorded = false;
        for (id, error) in failures {
            recorded |= tree
                .record_host_retirement_for_attempt(
                    &id,
                    expected.get(&id).and_then(Option::as_ref),
                    error,
                )
                .and_then(|value| self.apply_recorded(&mut tree, &id).map(|()| value))?;
        }
        self.save(&mut tree)?;
        drop(tree);
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

    /// The tree's guard, for a read that queues nothing.
    ///
    /// # Errors
    ///
    /// Refuses when the tree mutex is poisoned.
    pub(crate) fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ProductAgentTree>, TopologyError> {
        self.tree.lock().map_err(|_| Self::lock_poisoned())
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
        let tree = self.lock()?;
        Ok(work(&tree))
    }

    /// Reads the tree without waiting for its mutex.
    ///
    /// `None` when the tree is locked or poisoned — a caller rendering a view
    /// would rather skip a frame than block behind a turn.
    pub fn try_read<R>(&self, work: impl FnOnce(&ProductAgentTree) -> R) -> Option<R> {
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

    /// Locks the tree, applies `work`, and takes the effects it queued.
    pub fn mutate<R>(
        &self,
        work: impl FnOnce(&mut ProductAgentTree) -> R,
    ) -> Result<(R, Effects), TopologyError> {
        let mut tree = self.lock()?;
        let result = work(&mut tree);
        let effects = tree.take_effects();
        Ok((result, effects))
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
    pub fn mutate_and_settle<R>(
        &self,
        work: impl FnOnce(&mut ProductAgentTree) -> R,
    ) -> Result<(R, Settled), TopologyError> {
        let (result, effects) = self.mutate(|tree| {
            let result = work(tree);
            self.save(tree).map(|()| result)
        })?;
        let result = result?;
        let settled = self.settle(effects)?;
        Ok((result, settled))
    }

    /// Runs `effects` against the port with the mutex released, then
    /// re-locks once to record them and publishes the tree. Recording can
    /// owe more, so an acknowledged result save continues the drain. A
    /// refused result save retains follow-on effects without dispatching
    /// them. Every effect already run is recorded before reporting the error.
    /// Destructive removal requires an acknowledged preparation; a refused
    /// preparation retains the copy and records recovery without dispatch.
    pub fn settle(&self, mut effects: Effects) -> Result<Settled, TopologyError> {
        let mut settled = Settled::default();
        let mut first_save_error = None;
        while !effects.is_empty() {
            let refused = {
                let tree = self.lock()?;
                if tree.revision != tree.stored_revision
                    && (!effects.seals.is_empty()
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
                                receipt.terminal_response.is_none()
                                    && receipt.terminal_code.is_none()
                            })
                        });
                    if !admitted {
                        refused.push(ticket.clone());
                    }
                    admitted
                });
                refused
            };
            let mut completed = effects.run(self.vcs.as_ref());
            completed.refuse_unpublished_unmints(refused);
            let mut tree = self.lock()?;
            let more = tree.complete(completed);
            settled.adopts.extend(more.adopts);
            settled.hook_failures.extend(more.hook_failures);
            settled.seal_failures.extend(more.seal_failures);
            settled.cleanup_failures.extend(more.cleanup_failures);
            tree.lift_restored_pause_if_quiescent();
            if let Err(error) = self.save(&mut tree) {
                first_save_error = Some(error);
                // Completed effects remain recorded. Follow-on reservations
                // stay queued until a later successful publication.
                break;
            }
            effects = tree.take_effects();
        }
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

    pub(crate) fn fold_startup(&self) -> Result<Effects, TopologyError> {
        let mut tree = self.lock()?;
        if tree.revision != tree.stored_revision {
            self.save(&mut tree)?;
        }
        let targets = tree.startup_fold_targets();
        let lift_pause = tree.state == crate::protocol::TreeAdmissionState::RestoredPaused
            && tree.quiescence_refusal().is_none();
        if targets.is_empty() && tree.pending_close_folds.is_empty() && !lift_pause {
            return Ok(tree.take_effects());
        }
        self.fold_and_commit(&mut tree, |tree| {
            for agent in targets {
                tree.fold_due_in_place(&agent)?;
            }
            Ok(())
        })?;
        Ok(tree.take_effects())
    }

    /// Whole-tree publish as the store's compare-and-swap:
    /// refused with `StaleRevision` when another handle published this tree
    /// since it was loaded. Always sent — an operation receipt lands
    /// without a revision bump — and always safe: an unchanged revision is
    /// a compare-and-swap hit only for the handle the store acknowledged.
    pub fn save(&self, tree: &mut ProductAgentTree) -> Result<(), TopologyError> {
        let acknowledged = self.store.save(tree).map_err(TopologyError::from)?;
        tree.acknowledge_revision(acknowledged);
        for terminal in std::mem::take(&mut tree.pending_turn_usages).into_values() {
            if let Some(observer) = &self.observer {
                observer.turn_usage(terminal);
            }
        }
        Ok(())
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
        match work(tree).and_then(|result| {
            if tree.state == crate::protocol::TreeAdmissionState::RestoredPaused {
                for agent in tree.startup_fold_targets() {
                    tree.fold_due_in_place(&agent)?;
                }
            }
            tree.fold_reserved_closes_in_place()?;
            tree.refresh_epoch_after_close();
            tree.prune_closed_copies();
            tree.lift_restored_pause_if_quiescent();
            self.save(tree).map(|()| result)
        }) {
            Ok(result) => Ok(result),
            Err(error) => {
                *tree = snapshot;
                Err(error)
            }
        }
    }

    /// Withdraws an unfacted followup in one revision CAS. The caller holds
    /// the child session and startup reservation while reading `tail` and
    /// calling this method; a failed guard or save changes no live rows.
    /// The opening envelope and optional send failure receipt publish with it.
    ///
    /// # Errors
    /// Refuses a changed feed, another intent/effect, or a failed revision CAS.
    pub fn withdraw_unstarted_followup(
        &self,
        agent_id: &str,
        message_id: &str,
        session_id: &str,
        tail: u64,
        receipt: Option<(String, TopologyErrorCode)>,
    ) -> Result<(), TopologyError> {
        let mut tree = self.tree.lock().map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "tree lock is poisoned",
            )
        })?;
        self.fold_and_commit(&mut tree, |tree| {
            tree.withdraw_unstarted_followup(agent_id, message_id, session_id, tail)?;
            settle_followup_refusal(tree, agent_id, message_id, receipt)
        })
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
        self.save_host_intent(tree, agent_id)?;
        self.fold_and_commit(tree, |tree| {
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
        && Arc::ptr_eq(&runtime.tree, tree)
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
