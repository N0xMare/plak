use super::*;

pub(super) type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Default)]
pub(super) struct CreateStoreState {
    pub(super) persisted_objects: BTreeSet<RuntimeObjectAddress>,
    pub(super) receipt: Option<crate::CreateReceipt>,
    pub(super) loaded: Option<crate::LoadedBranch>,
    pub(super) stored: Option<StoredCheckpoint>,
    pub(super) commits: BTreeMap<CheckpointId, CommittedCheckpoint>,
    pub(super) stored_commits: BTreeMap<CheckpointId, StoredCheckpoint>,
    pub(super) active_lease: Option<BranchLease>,
    pub(super) released_lease: Option<BranchLease>,
    pub(super) last_fence: u64,
}

#[derive(Default)]
pub(super) struct TestCreateStore {
    pub(super) admission_jobs: Mutex<Option<Vec<&'static str>>>,
    pub(super) captured_admission_jobs: Mutex<Vec<&'static str>>,
    pub(super) admission_usage: AtomicBool,
    pub(super) reclaim_admission_pressure: AtomicBool,
    pub(super) state: Mutex<CreateStoreState>,
    pub(super) genesis: Mutex<Option<StoredCheckpoint>>,
    pub(super) feed_records: Mutex<Vec<crate::feed::FeedRecord>>,
    pub(super) feed_retained_from: AtomicU64,
    pub(super) feed_read_budgets: Mutex<Vec<usize>>,
    pub(super) feed_read_ranges: Mutex<Vec<(u64, usize)>>,
    pub(super) checkpoint_loads: AtomicUsize,
    pub(super) pruned_commits: Mutex<BTreeSet<CheckpointId>>,
    /// The feed facts of every create and commit request, in call order.
    pub(super) feed_facts: Mutex<Vec<Vec<crate::feed::FeedFact>>>,
    /// Every accepted commit's checkpoint with its feed facts, in order.
    pub(super) accepted_commits: Mutex<Vec<(Arc<RuntimeCheckpoint>, Vec<crate::feed::FeedFact>)>>,
    /// Halts every commit after the n-th (1-based) accepted one, as a crash
    /// right after that commit would.
    pub(super) halt_after_accepted_commit: AtomicUsize,
    pub(super) validate_candidate_children: AtomicBool,
    pub(super) block_after_completed_nested_effect: AtomicBool,
    pub(super) block_after_prepared_nested_child: AtomicBool,
    pub(super) lose_create_ack: AtomicBool,
    pub(super) lose_commit_ack_on: AtomicUsize,
    pub(super) lost_commit_acks: AtomicUsize,
    pub(super) commit_calls: AtomicUsize,
    pub(super) recover_commit_calls: AtomicUsize,
    pub(super) create_calls: AtomicUsize,
    pub(super) recover_calls: AtomicUsize,
    pub(super) releases: AtomicUsize,
    pub(super) flushes: AtomicUsize,
    pub(super) renew_calls: AtomicUsize,
    pub(super) hang_release_on: AtomicUsize,
    pub(super) hang_shutdown: AtomicBool,
    pub(super) provider_dispatched: Arc<AtomicBool>,
    pub(super) halt_after_checkpoint: Mutex<Option<CheckpointId>>,
    /// Blocks every commit after the first one that lands a model round.
    pub(super) block_after_completed_round: AtomicBool,
    pub(super) block_after_promoted_hosted: AtomicBool,
    pub(super) block_after_content_filter_guidance: AtomicBool,
    /// Promoted hosted continuations to let through before blocking commits.
    ///
    /// `block_after_promoted_hosted` stops at the first one. A turn that must be
    /// restored *above* its round cap has to get past an earlier pause first.
    pub(super) skip_promoted_hosted: AtomicUsize,
    pub(super) block_after_resolved_hosted_decision: AtomicBool,
    pub(super) block_after_hosted_effect_count: AtomicUsize,
    pub(super) commits_blocked: AtomicBool,
    pub(super) commits_halted: AtomicBool,
    pub(super) force_recover_commit_not_found: AtomicBool,
    pub(super) reject_next_commit: Mutex<Option<SessionStoreError>>,
    pub(super) fail_atomic_queued_cancel: AtomicBool,
    pub(super) fail_quiescent_terminal: AtomicBool,
    pub(super) refused_terminal:
        Mutex<Option<(Arc<RuntimeCheckpoint>, Vec<crate::feed::FeedFact>)>>,
    pub(super) fail_tool_exchange_promotion: AtomicBool,
    pub(super) block_process_notice_commits: AtomicBool,
    pub(super) process_notice_commit_started: Notify,
    pub(super) fail_provider_preparation: AtomicBool,
    /// Rejects the commit that prepares the second call of a tool batch.
    pub(super) fail_second_effect_preparation: AtomicBool,
    /// Refuses the n-th (1-based) ordinary commit at its admission threshold.
    pub(super) refuse_ordinary_commit_on: AtomicUsize,
    /// While set, the store reports its checkpoint bound exhausted for
    /// ordinary work and refuses every ordinary commit.
    pub(super) ordinary_exhausted: Mutex<Option<Arc<AtomicBool>>>,
    pub(super) ordinary_commits: AtomicUsize,
    pub(super) ordinary_refusals: AtomicUsize,
    pub(super) fail_nested_edge: AtomicBool,
    pub(super) rejected_nested_edge:
        Mutex<Option<(Arc<RuntimeCheckpoint>, Arc<RuntimeCheckpoint>)>>,
    pub(super) extend_lease_on_renew: AtomicBool,
    pub(super) reject_renewal: AtomicBool,
    /// Revives an expired lease under its fence, as a store does for a holder
    /// that was suspended rather than replaced.
    pub(super) allow_revival: AtomicBool,
    pub(super) revive_calls: AtomicUsize,
    pub(super) fail_lease_clock: AtomicBool,
    pub(super) lease_read_override: Mutex<Option<LeaseInstant>>,
    pub(super) delay_next_renewal_ack: AtomicBool,
    pub(super) renewal_clock: Mutex<Option<tokio::time::Instant>>,
    pub(super) publication_clock: Mutex<Option<(tokio::time::Instant, u64)>>,
    pub(super) delay_next_publication: AtomicBool,
    pub(super) lose_dispatched_ack: AtomicBool,
    pub(super) compaction_ack_only: AtomicBool,
    pub(super) delay_dispatched_ack: AtomicBool,
    pub(super) delayed_recovery_started: Notify,
    pub(super) delayed_recovery_release: Notify,
    pub(super) delay_recovery_once: AtomicBool,
    pub(super) publication_started: Notify,
    pub(super) publication_release: Notify,
    pub(super) expired_renewals: AtomicUsize,
    pub(super) renew_started: Arc<Notify>,
    pub(super) renew_gate: Arc<Notify>,
    pub(super) fail_prune: AtomicBool,
    pub(super) prune_calls: AtomicUsize,
    pub(super) block_terminal_at_revision: AtomicU64,
    pub(super) terminal_started: Notify,
    pub(super) terminal_release: Notify,
    pub(super) block_prune: AtomicBool,
    pub(super) prune_started: Notify,
    pub(super) prune_release: Notify,
    /// The horizon of the last acknowledged prune, reported by `load_branch`.
    pub(super) horizon: Mutex<Option<BranchRevision>>,
}

impl TestCreateStore {
    pub(super) fn record_admission_job(&self, name: &'static str) {
        let mut jobs = lock_mutex(&self.admission_jobs);
        if let Some(jobs) = jobs.as_mut() {
            jobs.push(name);
        }
        if name == "commit"
            && let Some(jobs) = jobs.take()
        {
            *lock_mutex(&self.captured_admission_jobs) = jobs;
        }
    }

    pub(super) fn with_lost_create_ack() -> Self {
        Self {
            lose_create_ack: AtomicBool::new(true),
            ..Self::default()
        }
    }

    pub(super) fn lose_commit_ack_on(&self, call: usize) {
        self.lose_commit_ack_on.store(call, Ordering::Release);
    }

    pub(super) fn force_recover_commit_not_found(&self) {
        self.force_recover_commit_not_found
            .store(true, Ordering::Release);
    }

    pub(super) fn hang_release_on(&self, call: usize) {
        self.hang_release_on.store(call, Ordering::Release);
    }

    /// Makes every renewal extend the active lease, exactly as the release
    /// stores do, and hold inside the store until [`Self::renew_gate`] fires.
    pub(super) fn extend_lease_on_renew(&self) {
        self.extend_lease_on_renew.store(true, Ordering::Release);
    }

    /// Fires once a gated renewal has entered the store.
    pub(super) fn renew_started(&self) -> Arc<Notify> {
        Arc::clone(&self.renew_started)
    }

    /// Releases a gated renewal that is holding inside the store.
    pub(super) fn renew_gate(&self) -> Arc<Notify> {
        Arc::clone(&self.renew_gate)
    }

    pub(super) fn active_lease(&self) -> Option<BranchLease> {
        lock_mutex(&self.state).active_lease.clone()
    }

    pub(super) fn released_lease(&self) -> Option<BranchLease> {
        lock_mutex(&self.state).released_lease.clone()
    }

    pub(super) fn expire_active_lease(&self) {
        let mut state = lock_mutex(&self.state);
        state.active_lease = None;
        state.released_lease = None;
    }

    pub(super) fn ordinary_is_exhausted(&self) -> bool {
        lock_mutex(&self.ordinary_exhausted)
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    pub(super) fn halt_after_checkpoint(&self, checkpoint_id: CheckpointId) {
        *lock_mutex(&self.halt_after_checkpoint) = Some(checkpoint_id);
    }

    pub(super) fn block_after_promoted_hosted(&self) {
        self.block_after_promoted_hosted
            .store(true, Ordering::Release);
    }

    pub(super) fn block_after_promoted_hosted_skipping(&self, skip: usize) {
        self.skip_promoted_hosted.store(skip, Ordering::Release);
        self.block_after_promoted_hosted
            .store(true, Ordering::Release);
    }

    pub(super) fn block_after_resolved_hosted_decision(&self) {
        self.block_after_resolved_hosted_decision
            .store(true, Ordering::Release);
    }

    pub(super) fn block_after_hosted_effect_count(&self, completed: usize) {
        self.block_after_hosted_effect_count
            .store(completed.saturating_add(1), Ordering::Release);
    }

    pub(super) fn resume_commits(&self) {
        self.commits_halted.store(false, Ordering::Release);
        self.commits_blocked.store(false, Ordering::Release);
    }

    pub(super) fn fail_atomic_queued_cancel(&self) {
        self.fail_atomic_queued_cancel
            .store(true, Ordering::Release);
    }

    pub(super) fn fail_quiescent_terminal(&self) {
        self.fail_quiescent_terminal.store(true, Ordering::Release);
    }

    pub(super) fn create_receipt(
        request: &CreateSession,
    ) -> Result<(crate::CreateReceipt, crate::LoadedBranch, StoredCheckpoint), SessionStoreError>
    {
        let checkpoint = request
            .initial()
            .validated_checkpoint()
            .map_err(|_| SessionStoreError::corrupt())?;
        let reference = CheckpointObjectRef::from_store(
            checkpoint.id(),
            request.initial().root(),
            checkpoint.integrity(),
        );
        let session = crate::SessionHead::from_store(
            request.session().session_id(),
            request.session().branch_id(),
            crate::SessionRevision::INITIAL,
            request.created_at(),
            checkpoint.manifest().durability(),
            false,
        );
        let branch = BranchHead::from_store(request.session(), BranchRevision::INITIAL, reference);
        let lease = BranchLease::from_store(
            request.session(),
            request.owner(),
            NonZeroU64::new(1).ok_or(SessionStoreError::corrupt())?,
            lease_instant(request.created_at().seconds().saturating_add(300), 0)
                .map_err(|_| SessionStoreError::corrupt())?,
            request.lease_seconds(),
        );
        let operation = crate::CreateOperation::from_store(
            request.session(),
            request.created_at(),
            request.owner(),
            request.lease_seconds(),
            reference,
            checkpoint.checkpoint_commit_identity(),
        );
        let receipt = crate::CreateReceipt::from_store(
            operation,
            session,
            branch,
            lease,
            crate::feed::FeedSeq::new(1),
        );
        let stored = StoredCheckpoint::from_store(reference, request.initial().clone())
            .map_err(|_| SessionStoreError::corrupt())?;
        let loaded = crate::LoadedBranch::from_store(session, branch, stored.clone());
        Ok((receipt, loaded, stored))
    }

    pub(super) fn matches_create(receipt: &crate::CreateReceipt, request: &CreateSession) -> bool {
        let operation = receipt.operation();
        operation.session() == request.session()
            && operation.created_at() == request.created_at()
            && operation.owner() == request.owner()
            && operation.lease_seconds() == request.lease_seconds()
            && operation.initial().checkpoint_id() == request.initial().checkpoint_id()
            && operation.initial().root() == request.initial().root()
            && operation.initial().integrity() == request.initial().checkpoint_integrity()
            && request
                .initial()
                .validated_checkpoint()
                .is_ok_and(|checkpoint| {
                    operation.checkpoint_commit_identity()
                        == checkpoint.checkpoint_commit_identity()
                })
    }

    pub(super) fn unsupported<T>() -> crate::StoreFuture<'static, T> {
        Box::pin(async { Err(SessionStoreError::BackendUnavailable) })
    }
}

impl SessionStore for TestCreateStore {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        if self.fail_lease_clock.load(Ordering::Acquire) {
            return Err(SessionStoreError::BackendUnavailable);
        }
        if let Some(now) = *lock_mutex(&self.lease_read_override) {
            return Ok(now);
        }
        let now = if let Some((epoch, base)) = *lock_mutex(&self.publication_clock) {
            base + epoch.elapsed().as_secs()
        } else if let Some(epoch) = *lock_mutex(&self.renewal_clock) {
            997 + epoch.elapsed().as_secs()
        } else {
            997
        };
        lease_instant(now, 0)
    }

    fn create(&self, request: CreateSession) -> crate::StoreFuture<'_, crate::CreateReceipt> {
        self.record_admission_job("create");
        self.create_calls.fetch_add(1, Ordering::Relaxed);
        lock_mutex(&self.feed_facts).push(request.feed_facts().to_vec());
        Box::pin(async move {
            let mut state = lock_mutex(&self.state);
            if state.receipt.is_some() {
                return Err(SessionStoreError::IdentityConflict);
            }
            let (receipt, loaded, stored) = Self::create_receipt(&request)?;
            lock_mutex(&self.feed_records).push(crate::feed::FeedRecord::from_store(
                crate::feed::FeedHeader {
                    session_id: request.session().session_id(),
                    branch_id: request.session().branch_id(),
                    seq: receipt.feed_seq(),
                    committed_at_ms: 0,
                    change: crate::feed::FeedChange::Created {
                        genesis: receipt.branch().checkpoint().checkpoint_id(),
                        revision: receipt.branch().revision(),
                    },
                },
                request.feed_facts().to_vec(),
            ));
            state.active_lease = Some(receipt.lease().clone());
            state.released_lease = None;
            state.last_fence = receipt.lease().fence().get();
            state.receipt = Some(receipt.clone());
            state.loaded = Some(loaded);
            state.persisted_objects.extend(
                request
                    .initial()
                    .supplied_objects()
                    .iter()
                    .map(crate::checkpoint_bundle::RuntimeObject::address),
            );
            *lock_mutex(&self.genesis) = Some(stored.clone());
            state.stored = Some(stored);
            if self.lose_create_ack.swap(false, Ordering::AcqRel) {
                Err(SessionStoreError::BackendUnavailable)
            } else {
                Ok(receipt)
            }
        })
    }

    fn recover_create(
        &self,
        request: CreateSession,
    ) -> crate::StoreFuture<'_, crate::CreateReceipt> {
        self.record_admission_job("recover_create");
        self.recover_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let state = lock_mutex(&self.state);
            let receipt = state.receipt.as_ref().ok_or(SessionStoreError::NotFound)?;
            if Self::matches_create(receipt, &request) {
                Ok(receipt.clone())
            } else {
                Err(SessionStoreError::IdentityConflict)
            }
        })
    }

    fn load_branch(&self, session: SessionRef) -> crate::StoreFuture<'_, crate::LoadedBranch> {
        self.record_admission_job("load_branch");
        Box::pin(async move {
            let loaded = lock_mutex(&self.state)
                .loaded
                .clone()
                .filter(|loaded| loaded.head().session() == session)
                .ok_or(SessionStoreError::NotFound)?;
            Ok(match *lock_mutex(&self.horizon) {
                Some(horizon) => loaded.with_horizon(horizon),
                None => loaded,
            })
        })
    }

    fn load_checkpoint(
        &self,
        checkpoint: CheckpointId,
    ) -> crate::StoreFuture<'_, StoredCheckpoint> {
        self.record_admission_job("load_checkpoint");
        self.checkpoint_loads.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let state = lock_mutex(&self.state);
            state
                .stored_commits
                .get(&checkpoint)
                .cloned()
                .or_else(|| {
                    state
                        .stored
                        .clone()
                        .filter(|stored| stored.reference().checkpoint_id() == checkpoint)
                })
                .or_else(|| {
                    lock_mutex(&self.genesis)
                        .clone()
                        .filter(|stored| stored.reference().checkpoint_id() == checkpoint)
                })
                .ok_or(SessionStoreError::NotFound)
        })
    }

    fn acquire(&self, request: AcquireBranch) -> crate::StoreFuture<'_, BranchLease> {
        self.record_admission_job("acquire");
        Box::pin(async move {
            let authority_now = *lock_mutex(&self.lease_read_override);
            let mut state = lock_mutex(&self.state);
            let loaded = state.loaded.as_ref().ok_or(SessionStoreError::NotFound)?;
            if loaded.head().session() != request.session() {
                return Err(SessionStoreError::NotFound);
            }
            if let Some(current) = state.active_lease.as_ref()
                && matches!(
                    authority_now
                        .map(|now| now.same_domain_cmp(current.expires_at()))
                        .transpose()?,
                    None | Some(std::cmp::Ordering::Less)
                )
            {
                return if current.owner() == request.owner()
                    && current.lease_seconds() == request.lease_seconds()
                {
                    Ok(current.clone())
                } else {
                    Err(SessionStoreError::LeaseHeld)
                };
            }
            let fence = state
                .last_fence
                .checked_add(1)
                .and_then(NonZeroU64::new)
                .ok_or(SessionStoreError::corrupt())?;
            let lease = BranchLease::from_store(
                request.session(),
                request.owner(),
                fence,
                authority_now.map_or_else(
                    || lease_instant(1_000, 0),
                    |now| now.checked_add_seconds(request.lease_seconds().get()),
                )?,
                request.lease_seconds(),
            );
            state.last_fence = fence.get();
            state.active_lease = Some(lease.clone());
            state.released_lease = None;
            Ok(lease)
        })
    }

    fn renew(&self, lease: BranchLease) -> crate::StoreFuture<'_, BranchLease> {
        self.record_admission_job("renew");
        self.renew_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if self.reject_renewal.load(Ordering::Acquire) {
                return Err(SessionStoreError::LeaseExpired);
            }
            let publication_clock = *lock_mutex(&self.publication_clock);
            if let Some((epoch, base)) = publication_clock {
                let now = base + epoch.elapsed().as_secs();
                let renewed = {
                    let mut state = lock_mutex(&self.state);
                    if state.active_lease.as_ref() != Some(&lease) {
                        return Err(SessionStoreError::FenceSuperseded);
                    }
                    if now >= lease.expires_at().seconds() {
                        self.expired_renewals.fetch_add(1, Ordering::Relaxed);
                        return Err(SessionStoreError::LeaseExpired);
                    }
                    let renewed = BranchLease::from_store(
                        lease.session(),
                        lease.owner(),
                        lease.fence(),
                        lease_instant(now + lease.lease_seconds().get(), 0)
                            .map_err(|_| SessionStoreError::corrupt())?,
                        lease.lease_seconds(),
                    );
                    state.active_lease = Some(renewed.clone());
                    renewed
                };
                if self.delay_next_renewal_ack.swap(false, Ordering::AcqRel) {
                    self.renew_started.notify_one();
                    self.renew_gate.notified().await;
                }
                return Ok(renewed);
            }
            let renewal_clock = *lock_mutex(&self.renewal_clock);
            if let Some(epoch) = renewal_clock {
                // Match a store that stamps expiry before a delayed commit acknowledgement.
                let now = 997 + epoch.elapsed().as_secs();
                let renewed = {
                    let mut state = lock_mutex(&self.state);
                    if state.active_lease.as_ref() != Some(&lease) {
                        return Err(SessionStoreError::FenceSuperseded);
                    }
                    if now >= lease.expires_at().seconds() {
                        self.expired_renewals.fetch_add(1, Ordering::Relaxed);
                        return Err(SessionStoreError::LeaseExpired);
                    }
                    let renewed = BranchLease::from_store(
                        lease.session(),
                        lease.owner(),
                        lease.fence(),
                        lease_instant(now + lease.lease_seconds().get(), 0)
                            .map_err(|_| SessionStoreError::corrupt())?,
                        lease.lease_seconds(),
                    );
                    state.active_lease = Some(renewed.clone());
                    renewed
                };
                self.renew_started.notify_one();
                if self.renew_calls.load(Ordering::Acquire) == 1 {
                    self.renew_gate.notified().await;
                }
                return Ok(renewed);
            }
            if self.extend_lease_on_renew.load(Ordering::Acquire) {
                self.renew_started.notify_one();
                self.renew_gate.notified().await;
                let mut state = lock_mutex(&self.state);
                if state.active_lease.as_ref() != Some(&lease) {
                    return Err(SessionStoreError::FenceSuperseded);
                }
                let renewed = BranchLease::from_store(
                    lease.session(),
                    lease.owner(),
                    lease.fence(),
                    lease_instant(lease.expires_at().seconds().saturating_add(1), 0)
                        .map_err(|_| SessionStoreError::corrupt())?,
                    lease.lease_seconds(),
                );
                state.active_lease = Some(renewed.clone());
                return Ok(renewed);
            }
            let state = lock_mutex(&self.state);
            if state.active_lease.as_ref() == Some(&lease) {
                Ok(lease)
            } else {
                Err(SessionStoreError::FenceSuperseded)
            }
        })
    }

    fn revive(&self, lease: BranchLease) -> crate::StoreFuture<'_, BranchLease> {
        self.record_admission_job("revive");
        self.revive_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if !self.allow_revival.load(Ordering::Acquire) {
                return Err(SessionStoreError::FenceSuperseded);
            }
            let mut state = lock_mutex(&self.state);
            if state.active_lease.as_ref() != Some(&lease)
                || state.last_fence != lease.fence().get()
            {
                return Err(SessionStoreError::FenceSuperseded);
            }
            let revived = BranchLease::from_store(
                lease.session(),
                lease.owner(),
                lease.fence(),
                lease_instant(
                    lease
                        .expires_at()
                        .seconds()
                        .saturating_add(lease.lease_seconds().get()),
                    0,
                )
                .map_err(|_| SessionStoreError::corrupt())?,
                lease.lease_seconds(),
            );
            state.active_lease = Some(revived.clone());
            Ok(revived)
        })
    }

    fn release(&self, lease: BranchLease) -> crate::StoreFuture<'_, ()> {
        self.record_admission_job("release");
        let call = self
            .releases
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        Box::pin(async move {
            if self.hang_release_on.load(Ordering::Acquire) == call {
                std::future::pending::<()>().await;
            }
            let mut state = lock_mutex(&self.state);
            if state.active_lease.as_ref() == Some(&lease) {
                state.active_lease = None;
                state.released_lease = Some(lease);
                return Ok(());
            }
            if state.active_lease.is_none() && state.released_lease.as_ref() == Some(&lease) {
                Ok(())
            } else {
                Err(SessionStoreError::FenceSuperseded)
            }
        })
    }

    fn commit(&self, request: CommitCheckpoint) -> crate::StoreFuture<'_, CommitReceipt> {
        self.record_admission_job("commit");
        let call = self
            .commit_calls
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        lock_mutex(&self.feed_facts).push(request.feed_facts().to_vec());
        Box::pin(async move {
            if let Some(error) = lock_mutex(&self.reject_next_commit).take() {
                return Err(error);
            }
            // SQLite checks every supplied object, including non-root intermediates.
            if self.validate_candidate_children.load(Ordering::Acquire) {
                let state = lock_mutex(&self.state);
                let mut supplied = state.persisted_objects.clone();
                supplied.extend(
                    request
                        .checkpoint()
                        .supplied_objects()
                        .iter()
                        .map(crate::checkpoint_bundle::RuntimeObject::address),
                );
                for object in request.checkpoint().supplied_objects() {
                    if object
                        .children()
                        .iter()
                        .any(|child| !supplied.contains(child))
                    {
                        return Err(SessionStoreError::Corrupt(
                            crate::session::StoreIntegrityReason::MissingChild,
                        ));
                    }
                }
            }
            if let Some((epoch, base)) = *lock_mutex(&self.publication_clock)
                && base + epoch.elapsed().as_secs() >= request.lease().expires_at().seconds()
            {
                return Err(SessionStoreError::LeaseExpired);
            }
            if self.delay_next_publication.swap(false, Ordering::AcqRel) {
                self.publication_started.notify_one();
                self.publication_release.notified().await;
            }
            if self.commits_blocked.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            if self.commits_halted.load(Ordering::Acquire) {
                return Err(SessionStoreError::BackendUnavailable);
            }
            if request.admission() == StoreAdmission::Ordinary && self.ordinary_is_exhausted() {
                self.ordinary_refusals.fetch_add(1, Ordering::AcqRel);
                return Err(SessionStoreError::admission_capacity(
                    StoreCapacityBound::Checkpoints,
                    57,
                    64,
                    StoreAdmission::Ordinary,
                ));
            }
            if request.admission() == StoreAdmission::Ordinary {
                let ordinary = self
                    .ordinary_commits
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1);
                if ordinary == self.refuse_ordinary_commit_on.load(Ordering::Acquire) {
                    self.ordinary_refusals.fetch_add(1, Ordering::AcqRel);
                    return Err(SessionStoreError::admission_capacity(
                        StoreCapacityBound::Checkpoints,
                        57,
                        64,
                        StoreAdmission::Ordinary,
                    ));
                }
            }
            let checkpoint = request
                .checkpoint()
                .validated_checkpoint()
                .map_err(|_| SessionStoreError::corrupt())?;
            if self.block_process_notice_commits.load(Ordering::Acquire)
                && checkpoint.manifest().canonical().history().segments().iter()
                    .flat_map(HistorySegment::items).flat_map(ConversationItem::blocks)
                    .any(|block| matches!(block, ContentBlock::Text(text) if text.text().contains("<process_notice")))
            {
                self.process_notice_commit_started.notify_one();
                std::future::pending::<()>().await;
            }
            if self.fail_tool_exchange_promotion.load(Ordering::Acquire)
                && checkpoint
                    .manifest()
                    .canonical()
                    .history()
                    .segments()
                    .iter()
                    .last()
                    .is_some_and(|segment| segment.kind() == HistorySegmentKind::ToolExchange)
            {
                self.fail_tool_exchange_promotion
                    .store(false, Ordering::Release);
                return Err(SessionStoreError::Invalid(
                    StoreInvalidReason::CheckpointCommitTopology,
                ));
            }
            if self.fail_nested_edge.load(Ordering::Acquire)
                && checkpoint
                    .manifest()
                    .ledger()
                    .operations()
                    .iter()
                    .any(|record| {
                        matches!(
                            record.kind(),
                            DurableOperationKind::CompositionChild {
                                request_sequence: 1,
                                ..
                            }
                        ) && matches!(record.state(), DurableOperationState::Completed(_))
                    })
            {
                let before = lock_mutex(&self.state)
                    .stored
                    .as_ref()
                    .ok_or(SessionStoreError::NotFound)?
                    .checkpoint()
                    .clone();
                *lock_mutex(&self.rejected_nested_edge) = Some((before, checkpoint.clone()));
                self.fail_nested_edge.store(false, Ordering::Release);
                self.commits_halted.store(true, Ordering::Release);
                return Err(SessionStoreError::BackendUnavailable);
            }
            if self.fail_provider_preparation.load(Ordering::Acquire)
                && checkpoint
                    .manifest()
                    .ledger()
                    .operations()
                    .iter()
                    .any(|record| {
                        matches!(record.kind(), DurableOperationKind::Provider { .. })
                            && matches!(record.state(), DurableOperationState::Prepared)
                    })
            {
                self.fail_provider_preparation
                    .store(false, Ordering::Release);
                return Err(SessionStoreError::Invalid(
                    StoreInvalidReason::CheckpointCommitTopology,
                ));
            }
            if self.fail_second_effect_preparation.load(Ordering::Acquire)
                && checkpoint
                    .manifest()
                    .ledger()
                    .operations()
                    .iter()
                    .any(|record| {
                        matches!(
                            record.kind(),
                            DurableOperationKind::Effect {
                                source_index: 1,
                                ..
                            }
                        ) && matches!(record.state(), DurableOperationState::Prepared)
                    })
            {
                self.fail_second_effect_preparation
                    .store(false, Ordering::Release);
                return Err(SessionStoreError::Invalid(
                    StoreInvalidReason::CheckpointCommitTopology,
                ));
            }
            if self.fail_atomic_queued_cancel.load(Ordering::Acquire)
                && checkpoint
                    .manifest()
                    .ledger()
                    .operations()
                    .into_iter()
                    .any(|record| {
                        matches!(record.kind(), DurableOperationKind::Command { .. })
                            && record.transitions().len() == 1
                            && matches!(record.state(), DurableOperationState::Finalized(_))
                            && serde_json::from_slice::<DurableCommandEnvelope>(
                                record.request().bytes(),
                            )
                            .is_ok_and(|envelope| {
                                matches!(
                                    envelope.effect,
                                    DurableActorCommandEffect::CancelRequested
                                )
                            })
                    })
            {
                self.fail_atomic_queued_cancel
                    .store(false, Ordering::Release);
                return Err(SessionStoreError::BackendUnavailable);
            }
            let terminal_revision = self.block_terminal_at_revision.load(Ordering::Acquire);
            if terminal_revision > 0
                && request.expected().revision().get() >= terminal_revision
                && matches!(
                    checkpoint.manifest().lifecycle(),
                    RuntimeLifecycle::Quiescent
                )
            {
                self.block_terminal_at_revision.store(0, Ordering::Release);
                self.terminal_started.notify_one();
                self.terminal_release.notified().await;
            }
            if self.fail_quiescent_terminal.load(Ordering::Acquire)
                && matches!(
                    checkpoint.manifest().lifecycle(),
                    RuntimeLifecycle::Quiescent
                )
            {
                self.fail_quiescent_terminal.store(false, Ordering::Release);
                *lock_mutex(&self.refused_terminal) =
                    Some((checkpoint.clone(), request.feed_facts().to_vec()));
                return Err(SessionStoreError::BackendUnavailable);
            }
            if checkpoint
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .any(|record| {
                    matches!(record.kind(), DurableOperationKind::Provider { .. })
                        && matches!(record.state(), DurableOperationState::Dispatched)
                })
            {
                self.provider_dispatched.store(true, Ordering::Release);
                let selected = !self.compaction_ack_only.load(Ordering::Acquire)
                    || checkpoint
                        .manifest()
                        .ledger()
                        .operations()
                        .into_iter()
                        .any(|record| {
                            matches!(
                                record.kind(),
                                DurableOperationKind::Provider {
                                    root: ModelOperationRoot::SessionWorkflow(_),
                                    ..
                                }
                            ) && matches!(record.state(), DurableOperationState::Dispatched)
                        });
                if selected && self.lose_dispatched_ack.swap(false, Ordering::AcqRel) {
                    self.lose_commit_ack_on.store(call, Ordering::Release);
                    self.delay_recovery_once.store(true, Ordering::Release);
                }
            }
            let delay_ack = self.provider_dispatched.load(Ordering::Acquire)
                && self.delay_dispatched_ack.swap(false, Ordering::AcqRel);
            let result =
                (|| {
                    let mut state = lock_mutex(&self.state);
                    let loaded = state.loaded.as_ref().ok_or(SessionStoreError::NotFound)?;
                    if loaded.head() != request.expected()
                        || request.lease().session() != request.expected().session()
                        || state.active_lease.as_ref() != Some(request.lease())
                    {
                        return Err(SessionStoreError::Conflict);
                    }
                    let reference = CheckpointObjectRef::from_store(
                        checkpoint.id(),
                        request.checkpoint().root(),
                        checkpoint.integrity(),
                    );
                    let revision = request
                        .expected()
                        .revision()
                        .get()
                        .checked_add(1)
                        .ok_or(SessionStoreError::corrupt())?;
                    let head = BranchHead::from_store(
                        request.expected().session(),
                        BranchRevision::from_store(revision),
                        reference,
                    );
                    let receipt = CommitReceipt::from_store(
                        reference,
                        request.expected(),
                        head,
                        crate::feed::FeedSeq::new(
                            lock_mutex(&self.feed_records)
                                .last()
                                .map_or(1, |record| record.header().seq.get() + 1),
                        ),
                    );
                    let committed = CommittedCheckpoint::from_store(
                        CommitOperation::from_store(
                            request.lease().clone(),
                            request.expected(),
                            reference,
                            checkpoint.checkpoint_commit_identity(),
                        ),
                        receipt,
                    );
                    let stored =
                        StoredCheckpoint::from_store(reference, request.checkpoint().clone())
                            .map_err(|_| SessionStoreError::corrupt())?;
                    let session = loaded.session();
                    state.loaded = Some(crate::LoadedBranch::from_store(
                        session,
                        head,
                        stored.clone(),
                    ));
                    state.persisted_objects.extend(
                        request
                            .checkpoint()
                            .supplied_objects()
                            .iter()
                            .map(crate::checkpoint_bundle::RuntimeObject::address),
                    );
                    state.stored = Some(stored.clone());
                    state
                        .stored_commits
                        .insert(reference.checkpoint_id(), stored);
                    state.commits.insert(reference.checkpoint_id(), committed);
                    lock_mutex(&self.feed_records).push(crate::feed::FeedRecord::from_store(
                        crate::feed::FeedHeader {
                            session_id: head.session().session_id(),
                            branch_id: head.session().branch_id(),
                            seq: receipt.feed_seq(),
                            committed_at_ms: 0,
                            change: crate::feed::FeedChange::Advanced {
                                parent: receipt.previous().checkpoint().checkpoint_id(),
                                checkpoint: reference.checkpoint_id(),
                                revision: head.revision(),
                                head: if matches!(
                                    checkpoint.manifest().lifecycle(),
                                    RuntimeLifecycle::Quiescent
                                ) {
                                    crate::feed::FeedHeadKind::Quiescent
                                } else {
                                    crate::feed::FeedHeadKind::Working
                                },
                            },
                        },
                        request.feed_facts().to_vec(),
                    ));
                    let accepted = {
                        let mut accepted = lock_mutex(&self.accepted_commits);
                        accepted.push((Arc::clone(&checkpoint), request.feed_facts().to_vec()));
                        accepted.len()
                    };
                    if accepted == self.halt_after_accepted_commit.load(Ordering::Acquire) {
                        self.commits_halted.store(true, Ordering::Release);
                    }
                    let promoted_hosted = matches!(
                        checkpoint.manifest().lifecycle(),
                        RuntimeLifecycle::Working {
                            recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                            obligations,
                            ..
                        } if obligations.len() == 2
                    );
                    let resolved_hosted_decision =
                        matches!(
                            checkpoint.manifest().lifecycle(),
                            RuntimeLifecycle::Working {
                                recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                                ..
                            }
                        ) && checkpoint.manifest().ledger().operations().into_iter().any(
                            |record| {
                                matches!(record.kind(), DurableOperationKind::EffectDecision { .. })
                                    && matches!(record.state(), DurableOperationState::Finalized(_))
                            },
                        ) && !checkpoint.manifest().ledger().operations().into_iter().any(
                            |record| matches!(record.kind(), DurableOperationKind::Effect { .. }),
                        );
                    let hosted_effect_target =
                        self.block_after_hosted_effect_count.load(Ordering::Acquire);
                    if self
                        .block_after_prepared_nested_child
                        .load(Ordering::Acquire)
                        && checkpoint
                            .manifest()
                            .ledger()
                            .operations()
                            .into_iter()
                            .any(|record| {
                                matches!(
                                    record.kind(),
                                    DurableOperationKind::CompositionChild { .. }
                                ) && matches!(record.state(), DurableOperationState::Prepared)
                            })
                    {
                        self.commits_blocked.store(true, Ordering::Release);
                    }
                    if self
                        .block_after_completed_nested_effect
                        .load(Ordering::Acquire)
                        && checkpoint
                            .manifest()
                            .ledger()
                            .operations()
                            .into_iter()
                            .any(|record| {
                                matches!(record.kind(), DurableOperationKind::Effect { .. })
                                    && matches!(record.state(), DurableOperationState::Completed(_))
                            })
                    {
                        self.commits_blocked.store(true, Ordering::Release);
                    }
                    let hosted_effect_count = checkpoint
                        .manifest()
                        .ledger()
                        .operations()
                        .into_iter()
                        .filter(|record| {
                            matches!(record.kind(), DurableOperationKind::Effect { .. })
                                && matches!(
                                    record.state(),
                                    DurableOperationState::Completed(_)
                                        | DurableOperationState::ProvenNotDispatched(_)
                                        | DurableOperationState::Finalized(_)
                                )
                        })
                        .count();
                    let hosted_tool_prefix =
                        hosted_effect_target != 0
                            && matches!(
                                checkpoint.manifest().lifecycle(),
                                RuntimeLifecycle::Working {
                                    recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                                    ..
                                }
                            )
                            && checkpoint.manifest().ledger().operations().into_iter().any(
                                |record| {
                                    matches!(
                                        record.kind(),
                                        DurableOperationKind::ToolBatchContinuation { .. }
                                    ) && matches!(
                                        record.state(),
                                        DurableOperationState::ContinuationRequired(_)
                                    )
                                },
                            )
                            && hosted_effect_count.saturating_add(1) == hosted_effect_target;
                    if self.block_after_completed_round.load(Ordering::Acquire)
                        && checkpoint
                            .manifest()
                            .ledger()
                            .operations()
                            .iter()
                            .any(|record| {
                                matches!(record.kind(), DurableOperationKind::Provider { .. })
                                    && matches!(record.state(), DurableOperationState::Completed(_))
                            })
                    {
                        self.commits_blocked.store(true, Ordering::Release);
                    }
                    if *lock_mutex(&self.halt_after_checkpoint) == Some(reference.checkpoint_id()) {
                        self.commits_halted.store(true, Ordering::Release);
                    }
                    if self
                        .block_after_content_filter_guidance
                        .load(Ordering::Acquire)
                        && checkpoint
                            .manifest()
                            .canonical()
                            .history()
                            .segments()
                            .iter()
                            .flat_map(HistorySegment::items)
                            .any(|item| {
                                item.actor() == Actor::User
                                    && item.provenance() == Provenance::Instruction
                                    && matches!(item.blocks(), [ContentBlock::Text(text)]
                                        if text.text().starts_with("<content_filter_guidance>"))
                            })
                    {
                        self.commits_blocked.store(true, Ordering::Release);
                    }
                    if self.block_after_promoted_hosted.load(Ordering::Acquire) && promoted_hosted {
                        if self.skip_promoted_hosted.load(Ordering::Acquire) == 0 {
                            self.commits_blocked.store(true, Ordering::Release);
                        } else {
                            self.skip_promoted_hosted.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    if self
                        .block_after_resolved_hosted_decision
                        .load(Ordering::Acquire)
                        && resolved_hosted_decision
                    {
                        self.commits_blocked.store(true, Ordering::Release);
                    }
                    if hosted_tool_prefix {
                        self.commits_blocked.store(true, Ordering::Release);
                    }
                    if self.lose_commit_ack_on.load(Ordering::Acquire) == call {
                        self.lost_commit_acks.fetch_add(1, Ordering::Relaxed);
                        Err(SessionStoreError::BackendUnavailable)
                    } else {
                        Ok(receipt)
                    }
                })();
            if delay_ack {
                self.delayed_recovery_started.notify_one();
                self.delayed_recovery_release.notified().await;
            }
            result
        })
    }

    fn recover_commit(
        &self,
        checkpoint: CheckpointId,
    ) -> crate::StoreFuture<'_, CommittedCheckpoint> {
        self.record_admission_job("recover_commit");
        self.recover_commit_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if self.delay_recovery_once.swap(false, Ordering::AcqRel) {
                self.delayed_recovery_started.notify_one();
                self.delayed_recovery_release.notified().await;
            }
            if self.force_recover_commit_not_found.load(Ordering::Acquire) {
                return Err(SessionStoreError::NotFound);
            }
            if lock_mutex(&self.pruned_commits).contains(&checkpoint) {
                return Err(SessionStoreError::Pruned);
            }
            lock_mutex(&self.state)
                .commits
                .get(&checkpoint)
                .cloned()
                .ok_or(SessionStoreError::NotFound)
        })
    }

    fn fork(&self, _request: ForkBranch) -> crate::StoreFuture<'_, ForkReceipt> {
        self.record_admission_job("fork");
        Self::unsupported()
    }

    fn recover_fork(&self, _request: RecoverFork) -> crate::StoreFuture<'_, ForkReceipt> {
        self.record_admission_job("recover_fork");
        Self::unsupported()
    }

    fn list(
        &self,
        _query: crate::SessionQuery,
    ) -> crate::StoreFuture<'_, crate::Page<crate::SessionSummary>> {
        self.record_admission_job("list");
        Self::unsupported()
    }

    fn reclaim(
        &self,
        mode: crate::session::StoreReclaimMode,
    ) -> crate::StoreFuture<'_, crate::session::StoreReclaimReport> {
        self.record_admission_job("reclaim");
        Box::pin(async move {
            if self.reclaim_admission_pressure.load(Ordering::Acquire) {
                assert_eq!(mode, crate::session::StoreReclaimMode::Bounded);
                lock_mutex(&self.ordinary_exhausted)
                    .as_ref()
                    .ok_or(SessionStoreError::corrupt())?
                    .store(false, Ordering::Release);
            }
            Ok(crate::session::StoreReclaimReport::default())
        })
    }

    fn usage(&self) -> crate::StoreFuture<'_, crate::StoreUsage> {
        if self.admission_usage.load(Ordering::Acquire) {
            let objects = if self.ordinary_is_exhausted() {
                StoreAdmission::Ordinary.threshold(64)
            } else {
                0
            };
            return Box::pin(
                async move { Ok(crate::StoreUsage::new((0, 64), (objects, 64), (0, 64))) },
            );
        }
        if !self.ordinary_is_exhausted() {
            return Self::unsupported();
        }
        Box::pin(async {
            Ok(crate::StoreUsage::new(
                (StoreAdmission::Ordinary.threshold(64), 64),
                (0, 64),
                (0, 64),
            ))
        })
    }

    fn archive(
        &self,
        _request: crate::ArchiveSession,
    ) -> crate::StoreFuture<'_, crate::ArchiveReceipt> {
        self.record_admission_job("archive");
        Self::unsupported()
    }

    fn delete(
        &self,
        _request: crate::DeleteSession,
    ) -> crate::StoreFuture<'_, crate::DeleteReceipt> {
        self.record_admission_job("delete");
        Self::unsupported()
    }

    fn recover_delete(
        &self,
        _request: crate::DeleteSession,
    ) -> crate::StoreFuture<'_, crate::DeleteReceipt> {
        self.record_admission_job("recover_delete");
        Self::unsupported()
    }

    fn read_feed(
        &self,
        _session_id: SessionId,
        after: crate::feed::FeedSeq,
        max_records: std::num::NonZeroUsize,
    ) -> crate::StoreFuture<'_, crate::feed::FeedPage> {
        self.record_admission_job("read_feed");
        lock_mutex(&self.feed_read_budgets).push(max_records.get());
        lock_mutex(&self.feed_read_ranges).push((after.get(), max_records.get()));
        Box::pin(async move {
            let records = lock_mutex(&self.feed_records);
            let tail = records
                .last()
                .map_or(crate::feed::FeedSeq::new(1), |record| record.header().seq);
            let retained_from = self.feed_retained_from.load(Ordering::Acquire).max(1);
            let window =
                crate::feed::FeedTail::from_store(crate::feed::FeedSeq::new(retained_from), tail);
            if after.get() < retained_from - 1 {
                return Ok(crate::feed::FeedPage::BehindRetention {
                    window,
                    heads: Vec::new(),
                });
            }
            Ok(crate::feed::FeedPage::Records {
                records: records
                    .iter()
                    .filter(|record| {
                        record.header().seq > after && record.header().seq.get() >= retained_from
                    })
                    .take(max_records.get())
                    .cloned()
                    .collect(),
                window,
            })
        })
    }

    fn feed_tail(&self, _session_id: SessionId) -> crate::StoreFuture<'_, crate::feed::FeedTail> {
        self.record_admission_job("feed_tail");
        Self::unsupported()
    }

    fn flush(&self) -> crate::StoreFuture<'_, crate::DurabilityReport> {
        self.record_admission_job("flush");
        self.flushes.fetch_add(1, Ordering::AcqRel);
        Box::pin(async {
            Ok(crate::DurabilityReport::from_store(
                RuntimeDurability::Required,
                0,
                None,
            ))
        })
    }

    fn shutdown(&self) -> crate::StoreFuture<'_, crate::DurabilityReport> {
        self.record_admission_job("shutdown");
        Box::pin(async move {
            if self.hang_shutdown.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            Ok(crate::DurabilityReport::from_store(
                RuntimeDurability::Required,
                0,
                None,
            ))
        })
    }

    fn prune(&self, request: crate::PruneBranch) -> crate::StoreFuture<'_, crate::PruneReceipt> {
        self.record_admission_job("prune");
        Box::pin(async move {
            self.prune_calls.fetch_add(1, Ordering::AcqRel);
            self.prune_started.notify_one();
            if self.block_prune.load(Ordering::Acquire) {
                self.prune_release.notified().await;
            }
            if self.fail_prune.load(Ordering::Acquire) {
                return Err(SessionStoreError::BackendUnavailable);
            }
            let horizon =
                BranchRevision::from_store(request.expected().revision().get().saturating_sub(1));
            *lock_mutex(&self.horizon) = Some(horizon);
            Ok(crate::PruneReceipt::from_store(
                request.expected(),
                horizon,
                0,
            ))
        })
    }
}
