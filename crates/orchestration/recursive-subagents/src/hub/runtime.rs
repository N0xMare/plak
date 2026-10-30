use super::{
    Any, Arc, AtomicBool, Future, HashSet, LiveCancelError, LiveCancelOutcome, LiveCancelReport,
    LiveChildHandle, LiveChildTable, LiveRetirementFailure, Mutex, Ordering, ProductAgentTree,
    ProductTopologyRuntime, RETIRE_JOIN_EXPIRED, RecoveryPassRequest, RecoveryPassSlot,
    RecyclingGuard, RetirementJoin, TopologyObserver, TreeDriver, TreeStore, TurnId, VcsOps,
    begin_retire_live, retire_live,
};

impl ProductTopologyRuntime {
    /// Installs the host's recovery trigger for returned startup/retirement owners.
    /// The callback must not retain this runtime; shutdown closes pass admission.
    pub async fn set_retirement_recovery(&self, recovery: Arc<dyn Fn() + Send + Sync>) {
        self.driver.set_retirement_recovery(recovery.clone());
        self.live.lock().await.retirement_recovery = Some(recovery);
    }

    /// Takes ownership of `tree`, binding it to `store` (one connection for
    /// the runtime's life) and `vcs` (installed as the tree's port).
    pub fn new(
        tree: ProductAgentTree,
        store: Arc<dyn TreeStore>,
        vcs: Arc<dyn VcsOps>,
        observer: Option<Arc<dyn TopologyObserver>>,
    ) -> Self {
        let tree = Arc::new(Mutex::new(tree));
        let driver = TreeDriver::new(tree, store, vcs, observer.clone());
        Self {
            live: Arc::new(tokio::sync::Mutex::new(LiveChildTable::default())),
            driver,
            closed: AtomicBool::new(false),
            reattach: tokio::sync::Mutex::new(()),
            recycling: Mutex::new(HashSet::new()),
            recovery_pass: Mutex::new(RecoveryPassSlot::default()),
            rejoin_spec: Mutex::new(None),
            observer,
        }
    }

    /// The tree with its store and VCS port. Every mutation, effect and
    /// publish for this tree goes through it.
    pub fn driver(&self) -> &TreeDriver {
        &self.driver
    }

    /// Raw tree access for synthetic fixtures; absent from production builds.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn tree_handle_for_test(&self) -> &Arc<Mutex<ProductAgentTree>> {
        self.driver.tree_handle_for_test()
    }

    /// Reads this tree under its mutex.
    ///
    /// # Errors
    ///
    /// Refuses when the tree mutex is poisoned.
    pub fn read<R>(
        &self,
        work: impl FnOnce(&ProductAgentTree) -> R,
    ) -> Result<R, crate::tree::TopologyError> {
        self.driver.read(work)
    }

    /// Reads this tree without waiting for its mutex; `None` when it is held.
    pub fn try_read<R>(&self, work: impl FnOnce(&ProductAgentTree) -> R) -> Option<R> {
        self.driver.try_read(work)
    }

    /// One mutation on this tree, with everything it queued settled.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned mutex and reports a failed settle.
    pub fn mutate_and_settle<R>(
        &self,
        work: impl FnOnce(&mut ProductAgentTree) -> R,
    ) -> Result<(R, crate::tree::Settled), crate::tree::TopologyError> {
        self.driver.mutate_and_settle(work)
    }

    /// One mutation, published, and everything it queued settled — the whole
    /// cycle in one call, so none of the three halves can be forgotten.
    ///
    /// This is what a caller above this crate reaches for: `work` gets the
    /// tree under its mutex, the tree is published while still locked, and
    /// the effects the mutation queued run with the lock released.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned mutex, a refused publish, and a failed settle;
    /// `work`'s own refusal comes back unchanged. Refused consequences are
    /// rolled back and no unacknowledged effect runs.
    pub fn mutate_then_emit<R>(
        &self,
        epoch_advanced: bool,
        work: impl FnOnce(&mut ProductAgentTree) -> Result<R, String>,
    ) -> Result<R, String> {
        self.mutate_then_emit_typed(epoch_advanced, |tree| {
            work(tree).map_err(|message| {
                crate::tree::TopologyError::new(
                    crate::protocol::TopologyErrorCode::OperationRecoveryRequired,
                    message,
                )
            })
        })
        .map_err(|error| error.message)
    }

    /// Publishes and settles one mutation while preserving typed tree refusals.
    ///
    /// # Errors
    /// Refuses a poisoned mutex, failed mutation, publication or settlement.
    /// A refused publication rolls back consequences without draining effects.
    pub fn mutate_then_emit_typed<R>(
        &self,
        epoch_advanced: bool,
        work: impl FnOnce(&mut ProductAgentTree) -> Result<R, crate::tree::TopologyError>,
    ) -> Result<R, crate::tree::TopologyError> {
        let (result, effects) = {
            let mut tree = self.driver.lock().map_err(|_| {
                crate::tree::TopologyError::new(
                    crate::protocol::TopologyErrorCode::OperationRecoveryRequired,
                    "topology tree lock is poisoned",
                )
            })?;
            if tree.revision != tree.stored_revision {
                self.driver.save_guarded(&mut tree)?;
            }
            let result = self.driver.fold_and_commit_guarded(&mut tree, work)?;
            if epoch_advanced && let Some(observer) = self.observer.as_ref() {
                observer.epoch_advanced(&tree.tree_id);
            }
            (result, tree.take_effects())
        };
        self.driver.settle(effects)?;
        Ok(result)
    }

    /// Installs process-local ownership before the consequence snapshot.
    /// `prepare` must change no durable rows or reserve external effects.
    /// Its registrations remain live on a refused save until their owners drop.
    ///
    /// # Errors
    /// Reports failed preparation, publication, or effect settlement.
    pub fn prepare_then_emit<R>(
        &self,
        prepare: impl FnOnce(&mut ProductAgentTree) -> Result<R, String>,
    ) -> Result<R, String> {
        let (result, effects) = {
            let mut tree = self.driver.lock().map_err(|error| error.message)?;
            if tree.revision != tree.stored_revision {
                self.driver
                    .save_guarded(&mut tree)
                    .map_err(|error| error.message)?;
            }
            let result = prepare(&mut tree)?;
            self.driver
                .fold_and_commit_guarded(&mut tree, |_| Ok(()))
                .map_err(|error| error.message)?;
            (result, tree.take_effects())
        };
        self.driver.settle(effects).map_err(|error| error.message)?;
        Ok(result)
    }

    /// Records and publishes an intent before folding its consequences.
    /// Process-local ownership changes in `record` are outside the revert
    /// boundary. The callback must not publish the tree itself.
    ///
    /// # Errors
    /// Reports a failed recording, either publish, or effect settlement.
    pub fn record_then_apply<R>(
        &self,
        agent_id: &str,
        record: impl FnOnce(&mut ProductAgentTree) -> Result<R, crate::tree::TopologyError>,
    ) -> Result<R, String> {
        let (result, effects) = {
            let mut tree = self.driver.lock().map_err(|error| error.message)?;
            let result = record(&mut tree).map_err(|error| error.message)?;
            self.driver
                .apply_recorded_guarded(&mut tree, agent_id)
                .map_err(|error| error.message)?;
            (result, tree.take_effects())
        };
        self.driver.settle(effects).map_err(|error| error.message)?;
        Ok(result)
    }

    /// Fenced whole-tree publish of the runtime's tree; `epoch_advanced`
    /// reports the install to the observer once the rows are durable, never
    /// before, so nothing is announced that a refused publish did not keep.
    pub fn save_then_emit(
        &self,
        tree: &mut ProductAgentTree,
        epoch_advanced: bool,
    ) -> Result<(), String> {
        self.driver.save(tree).map_err(|error| error.message)?;
        if epoch_advanced && let Some(observer) = self.observer.as_ref() {
            observer.epoch_advanced(&tree.tree_id);
        }
        Ok(())
    }

    /// This tree's telemetry port, when one was installed.
    pub fn observer(&self) -> Option<Arc<dyn TopologyObserver>> {
        self.observer.as_ref().map(Arc::clone)
    }

    /// Observes owner-confirmed retirement failures, independently of their initiator.
    pub fn observe_retirement_faults(&self) -> tokio::sync::broadcast::Receiver<(String, String)> {
        self.driver.retirement_faults.subscribe()
    }

    /// Parks the host's rejoin spec on the hub for this tree's lifetime.
    ///
    /// Opaque on purpose: what a spawn's parent *is* — its session, its
    /// account, its composition — is entirely the host's, and the hub only
    /// needs somewhere to keep it that dies when the hub does. Storing it as
    /// `Any` is what makes that literally true: the tree cannot read it even
    /// by accident, and [`Self::rejoin_spec`] hands it back to the one caller
    /// that knows its type.
    pub fn remember_rejoin_spec(&self, parent: Arc<dyn Any + Send + Sync>) {
        if let Ok(mut slot) = self.rejoin_spec.lock() {
            *slot = Some(parent);
        }
    }

    /// The parked rejoin spec, for the host to downcast to its own type.
    pub fn rejoin_spec(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.rejoin_spec
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(Arc::clone))
    }

    /// Whether this hub has been closed for shutdown. A closed hub admits no
    /// live turn, which is what makes the drain final.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Serializes crash-cut reattach so overlapping root attaches wait for
    /// `resume_tree` instead of exposing a still-paused tree.
    pub async fn lock_reattach(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.reattach.lock().await
    }

    /// Marks `agent_id` as owned by its turn waiter's recycle, which runs
    /// without the reattach lock. Set under that lock before the live entry
    /// goes, so a recovery never sees the child unowned mid-recycle. The mark
    /// lasts until the returned guard drops, so a recycle that panics or is
    /// cancelled cannot leave the child owned for good.
    #[must_use = "the mark is cleared when the guard drops"]
    pub fn mark_recycling(self: &Arc<Self>, agent_id: &str) -> RecyclingGuard {
        if let Ok(mut recycling) = self.recycling.lock() {
            recycling.insert(agent_id.to_owned());
        }
        RecyclingGuard {
            runtime: Arc::clone(self),
            agent_id: agent_id.to_owned(),
        }
    }

    /// Whether a turn waiter is recycling `agent_id` right now.
    pub fn is_recycling(&self, agent_id: &str) -> bool {
        self.recycling
            .lock()
            .is_ok_and(|recycling| recycling.contains(agent_id))
    }

    /// Starts this tree's background recovery pass with `spawn`. While one is
    /// running, the request is not dropped: the running pass runs once more
    /// before it finishes (see [`Self::finish_recovery_pass`]). A closed hub
    /// starts nothing.
    pub fn start_recovery_pass(
        &self,
        spawn: impl FnOnce() -> tokio::task::JoinHandle<()>,
    ) -> RecoveryPassRequest {
        let Ok(mut slot) = self.recovery_pass.lock() else {
            return RecoveryPassRequest::Closed;
        };
        if self.is_closed() {
            return RecoveryPassRequest::Closed;
        }
        if slot.busy() {
            slot.rerun = true;
            return RecoveryPassRequest::Rerun;
        }
        slot.rerun = false;
        slot.running = true;
        slot.task = Some(spawn());
        RecoveryPassRequest::Started
    }

    /// Called by a pass when it is done: `true` when a request arrived while
    /// it ran and it must run once more; otherwise the slot is released.
    /// Decided under the slot lock, so no request can fall between the
    /// answer and the pass ending.
    pub fn finish_recovery_pass(&self) -> bool {
        let Ok(mut slot) = self.recovery_pass.lock() else {
            return false;
        };
        if slot.rerun && !self.is_closed() {
            slot.rerun = false;
            return true;
        }
        slot.running = false;
        false
    }

    /// Whether this tree's background recovery pass is still running.
    pub fn recovery_pass_running(&self) -> bool {
        self.recovery_pass.lock().is_ok_and(|slot| slot.busy())
    }

    /// The reattach lock without waiting, so a test can prove a second
    /// reattach is actually excluded rather than merely slow.
    #[cfg(any(test, feature = "test-support"))]
    pub fn try_lock_reattach(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, ()>, tokio::sync::TryLockError> {
        self.reattach.try_lock()
    }

    /// Stores `child` if this agent is vacant and the hub is still open.
    /// Returns `Err(child)` when the caller must `release_opened` the loser.
    /// The one admission decision for a live turn, taken with the live map
    /// held. A closed tree admits nothing, and one agent holds
    /// at most one turn.
    ///
    /// This is the invariant the live table must keep when it moves to the
    /// product: the closed flag is read *under the same lock the insert
    /// takes*, and `shutdown_runtime` sets that flag before it takes the lock
    /// to drain. So a turn either wins the lock before the drain — and is in
    /// the set the drain takes — or loses it and reads the flag the drain
    /// already set, and is refused. There is no third outcome, and no turn is
    /// left running with nothing holding it.
    pub(super) fn admits_live(&self, live: &LiveChildTable, agent_id: &str) -> bool {
        !self.is_closed() && !live.contains_key(agent_id)
    }

    /// Takes the live slot for one agent, or hands `child` back when the slot
    /// is taken or the hub is closed.
    ///
    /// The check and the insert happen under one hold of the live lock, so
    /// two turns racing for one agent cannot both win; the loser is returned
    /// rather than dropped, because only its owner can release it.
    ///
    /// # Errors
    ///
    /// Returns the rejected handle when the agent is occupied or the hub is
    /// closed.
    pub async fn try_occupy_live(
        &self,
        agent_id: String,
        child: Box<dyn LiveChildHandle>,
    ) -> Result<TurnId, Box<dyn LiveChildHandle>> {
        self.try_occupy_live_admitted(agent_id, child, || Ok(()))
            .await
            .map_err(|(child, _)| child)
    }

    /// [`Self::try_occupy_live`], admitted only if `admit` (which may
    /// publish the turn's binding) accepts under the live lock. An
    /// interrupt's cancel needs that lock, so it either lands before `admit`
    /// runs or finds and cancels the occupied turn.
    ///
    /// # Errors
    ///
    /// Returns the rejected handle, with `admit`'s refusal when it refused.
    pub async fn try_occupy_live_admitted(
        &self,
        agent_id: String,
        child: Box<dyn LiveChildHandle>,
        admit: impl FnOnce() -> Result<(), String>,
    ) -> Result<TurnId, (Box<dyn LiveChildHandle>, Option<String>)> {
        if self.is_closed() {
            return Err((child, None));
        }
        let mut live = self.live.lock().await;
        if !self.admits_live(&live, &agent_id) {
            return Err((child, None));
        }
        if let Err(refusal) = admit() {
            return Err((child, Some(refusal)));
        }
        let turn_id = child.turn_id();
        if let Err(error) = self
            .driver
            .mutate(|tree| tree.bind_turn_owner(&agent_id, turn_id.to_string()))
        {
            return Err((child, Some(error.message)));
        }
        live.children.insert(agent_id, child);
        Ok(turn_id)
    }

    /// Marks the tree closed: the flag every later admission reads. Set
    /// before the drain takes the live lock, never after.
    pub(super) fn close_for_shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.driver.shutdown_storage_waits();
    }

    /// Cancels or joins the retirement already owning this agent slot.
    pub async fn cancel_live(&self, agent_id: &str) -> Result<LiveCancelOutcome, LiveCancelError> {
        self.driver.cancel_storage_waits(agent_id);
        retire_live(&self.driver, &self.live, agent_id, None, true).await
    }

    /// Cancels live children concurrently, reporting in input order (close
    /// supplies postorder). Each exact retirement keeps its own bound.
    ///
    /// Every agent lands in exactly one bucket: cancelled, not live,
    /// owner-confirmed failure, or joiner expiry. The close path needs
    /// not-live separated from cancelled —
    /// "there was nothing running" and "the turn was stopped" are different
    /// answers, and folding them into one `Ok(())` hid the difference.
    pub async fn cancel_live_many(&self, agent_ids: &[String]) -> LiveCancelReport {
        self.begin_cancellation(agent_ids, true).await.await
    }

    /// Signals the exact owners after the caller's durable close/interrupt.
    /// The returned join owns their outcomes even if a slot settles or is reused.
    pub async fn begin_cancel_live_many(
        &self,
        agent_ids: &[String],
    ) -> impl Future<Output = LiveCancelReport> + Send + 'static {
        self.begin_cancellation(agent_ids, false).await
    }

    async fn begin_cancellation(
        &self,
        agent_ids: &[String],
        bounded: bool,
    ) -> impl Future<Output = LiveCancelReport> + Send + 'static {
        let joins = futures_util::future::join_all(agent_ids.iter().map(|agent_id| async move {
            let tree_id = self
                .driver
                .read(|tree| tree.tree_id.clone())
                .unwrap_or_default();
            self.driver.cancel_storage_waits(agent_id);
            let join = begin_retire_live(&self.driver, &self.live, agent_id, None, true).await;
            let (expected_attempt_id, expected_turn_id) = match &join {
                RetirementJoin::Startup(startup) => (startup.expected_attempt.clone(), None),
                RetirementJoin::Live { retirement, .. } => {
                    (retirement.expected_attempt.clone(), Some(retirement.turn))
                }
                RetirementJoin::NotLive => (None, None),
            };
            (
                tree_id,
                agent_id.clone(),
                expected_attempt_id,
                expected_turn_id,
                join,
            )
        }))
        .await;
        async move {
            let mut report = LiveCancelReport::default();
            let outcomes = futures_util::future::join_all(joins.into_iter().map(
                |(tree_id, agent_id, expected_attempt_id, expected_turn_id, join)| async move {
                    let outcome = if bounded {
                        join.join().await
                    } else {
                        join.join_owned().await
                    };
                    (
                        tree_id,
                        agent_id,
                        expected_attempt_id,
                        expected_turn_id,
                        outcome,
                    )
                },
            ))
            .await;
            for (tree_id, agent_id, expected_attempt_id, expected_turn_id, outcome) in outcomes {
                match outcome {
                    Ok(LiveCancelOutcome::Cancelled) => report.cancelled.push(agent_id.clone()),
                    Ok(LiveCancelOutcome::NotLive) => report.not_live.push(agent_id.clone()),
                    Err(LiveCancelError::JoinExpired) => {
                        report.joiner_expired.push(agent_id.clone());
                        report
                            .first_failure
                            .get_or_insert_with(|| RETIRE_JOIN_EXPIRED.to_owned());
                    }
                    Err(LiveCancelError::OwnerFailure(message)) => {
                        report.failed.push(agent_id.clone());
                        report.owner_failures.push(LiveRetirementFailure {
                            tree_id,
                            agent_id: agent_id.clone(),
                            expected_attempt_id,
                            expected_turn_id,
                            message: message.clone(),
                        });
                        if report.first_failure.is_none() {
                            report.first_failure = Some(message);
                        }
                    }
                }
            }
            report
        }
    }
}
