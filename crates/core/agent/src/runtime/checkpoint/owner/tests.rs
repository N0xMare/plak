use super::*;
use crate::checkpoint_bundle::tests::{
    fixture_with_history, fixture_with_history_and_ledger, history_segment, without_cached_objects,
};
use crate::internal::{self as store_api, LeaseDomain, LeaseInstant, RuntimeObject};
use crate::runtime::ToolExecutionResult;
use crate::runtime::continuation::runtime_component;
use crate::runtime::driver::DurableUndispatchedToolRequest;
use crate::runtime::operation_record::{tool_operation_terminal_state, working_lifecycle};
use crate::*;
use plakous_model::{
    AttemptId, CallId, CanonicalHistory, DigestDomain, EntryId, ToolCallBlock, ToolCallState,
    ToolCorrelation, ToolExecutionOwner, ToolResultBlock, ToolResultKind,
};
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::Weak;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn abandoned_undispatched_call_is_named(proven_not_dispatched: bool) -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(900))?;
    let call = ToolCallBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(902))?,
        ToolName::new("loopback_effect")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        "{}",
    )?;
    let result = ToolResultBlock::new(
        call.call_id(),
        ToolExecutionOwner::Local,
        ToolResultKind::Failure,
        plakous_tools::ToolOutcome::failure_text("superseded by steering")?
            .parts()
            .to_vec(),
        call.correlation().clone(),
    )?;
    let execution = ToolExecutionResult::new(result, false);
    let binding = Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"undispatched"])?;
    let request: DurableUndispatchedToolRequest = (
        "proven-not-dispatched-tool-v1".to_owned(),
        binding,
        call.clone(),
        EntryId::try_from(uuid::Uuid::from_u128(903))?,
        execution.clone(),
    );
    let bytes = serde_json::to_vec(&request)?;
    let component = runtime_component(RuntimeComponentKind::Operation, bytes.clone())?;
    let mut record = DurableOperationRecord::new(
        1,
        DurableOperationKind::Effect {
            turn_id,
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(901))?,
            call_id: call.call_id(),
            source_index: 0,
        },
        component,
        vec![DurableOperationTransition::new(
            CheckpointId::try_from(uuid::Uuid::from_u128(910))?,
            DurableOperationState::Prepared,
        )],
    )?;
    if proven_not_dispatched {
        record = record.append_transition(DurableOperationTransition::new(
            CheckpointId::try_from(uuid::Uuid::from_u128(911))?,
            tool_operation_terminal_state(&Ok(execution), false, request.3)?,
        ))?;
    }
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![record])?;
    let lifecycle = working_lifecycle(turn_id, &ledger)?;
    let checkpoint = fixture_with_history_and_ledger(
        CanonicalHistory::default(),
        1,
        true,
        Some(ledger),
        Some(lifecycle),
    )?;
    assert_eq!(
        uncommitted_tool_calls(&checkpoint, turn_id).map_err(|error| error.to_string())?,
        vec![(call.call_id(), call.exposed_name().clone())]
    );
    let retained = checkpoint
        .manifest()
        .ledger()
        .operation(1)
        .ok_or("effect")?;
    assert_eq!(retained.request().bytes(), bytes);
    assert_eq!(
        matches!(
            retained.state(),
            DurableOperationState::ProvenNotDispatched(_)
        ),
        proven_not_dispatched
    );
    Ok(())
}

#[test]
fn abandoned_prepared_undispatched_call_is_named_without_dispatch() -> TestResult {
    abandoned_undispatched_call_is_named(false)
}

#[test]
fn abandoned_proven_not_dispatched_call_is_named_without_dispatch() -> TestResult {
    abandoned_undispatched_call_is_named(true)
}

/// A deterministic CAS sweep with optional lost acknowledgement. Commit
/// verification uses only supplied objects and the surviving CAS, never
/// the publication's warm cache.
#[derive(Default)]
struct SweepStore {
    objects: Mutex<BTreeMap<RuntimeObjectAddress, RuntimeObject>>,
    authority: Mutex<Weak<Mutex<CheckpointAuthority>>>,
    lose_ack: Mutex<bool>,
    calls: Mutex<usize>,
}

impl SweepStore {
    fn accept(&self, bundle: &CheckpointObjectBundle) -> TestResult {
        let mut objects = lock_mutex(&self.objects);
        for object in bundle.supplied_objects() {
            objects.insert(object.address(), object.clone());
        }
        let closure = Self::closure(&objects, bundle.root())?;
        CheckpointObjectBundle::from_store(
            bundle.checkpoint_id(),
            bundle.checkpoint_integrity(),
            bundle.root(),
            closure.into_values().collect(),
        )?;
        Ok(())
    }

    fn closure(
        objects: &BTreeMap<RuntimeObjectAddress, RuntimeObject>,
        root: RuntimeObjectAddress,
    ) -> TestResult<BTreeMap<RuntimeObjectAddress, RuntimeObject>> {
        let mut closure = BTreeMap::new();
        let mut pending = vec![root];
        while let Some(address) = pending.pop() {
            if closure.contains_key(&address) {
                continue;
            }
            let object = objects.get(&address).ok_or("missing committed object")?;
            pending.extend_from_slice(object.children());
            closure.insert(address, object.clone());
        }
        Ok(closure)
    }
}

impl SessionStore for SweepStore {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 0, 0)
    }

    plakous_test_support::session_store_methods! {
        unavailable store_api;
        create, recover_create, load_branch, load_checkpoint, acquire, renew, release,
        commit, recover_commit, fork, recover_fork, list, archive, delete, recover_delete,
        feed_tail, branch_line, load_line, line_job_terminals, record_line_job_terminals,
        read_feed, usage, flush,
    }

    fn shutdown(&self) -> StoreFuture<'_, DurabilityReport> {
        self.flush()
    }

    fn prune(&self, request: PruneBranch) -> StoreFuture<'_, PruneReceipt> {
        Box::pin(async move {
            *lock_mutex(&self.calls) += 1;
            let state = lock_mutex(&self.authority)
                .upgrade()
                .ok_or(SessionStoreError::corrupt())?;
            let authority = lock_mutex(&state);
            let mut objects = lock_mutex(&self.objects);
            let closure = Self::closure(&objects, authority.bundle.root())
                .map_err(|_| SessionStoreError::corrupt())?;
            // The cache must be narrowed before dispatch, not on receipt.
            assert_eq!(authority.bundle.stats().object_count(), closure.len());
            *objects = closure;
            if *lock_mutex(&self.lose_ack) {
                return Err(SessionStoreError::BackendUnavailable);
            }
            Ok(PruneReceipt::from_store(
                request.expected(),
                BranchRevision::from_store(request.expected().revision().get() - 1),
                1,
            ))
        })
    }
}

fn checkpoint(revision: u128, tail: usize) -> TestResult<Arc<RuntimeCheckpoint>> {
    // Ballast ensures the warm cache remains below both amortized thresholds.
    let mut segments = (0..32)
        .map(history_segment)
        .collect::<TestResult<Vec<_>>>()?;
    segments.push(history_segment(tail)?);
    Ok(Arc::new(fixture_with_history(
        CanonicalHistory::new(segments)?,
        revision,
        false,
    )?))
}

fn install(
    owner: &CheckpointOwner,
    checkpoint: Arc<RuntimeCheckpoint>,
    bundle: CheckpointObjectBundle,
) {
    let mut authority = lock_mutex(&owner.state);
    let topology = checkpoint.manifest().topology();
    let reference = CheckpointObjectRef::from_store(
        topology.checkpoint_id,
        bundle.root(),
        checkpoint.integrity(),
    );
    if let Some(branch) = authority.durable.as_mut() {
        branch.head = BranchHead::from_store(
            branch.head.session(),
            BranchRevision::from_store(topology.branch_revision),
            reference,
        );
    }
    authority.checkpoint = checkpoint;
    authority.bundle = bundle;
}

#[tokio::test]
async fn warm_owner_reconstructs_reclaimed_addresses_even_after_lost_prune_ack() -> TestResult {
    let first = checkpoint(1, 1_000)?;
    let bundle = RuntimeCheckpointCodec::bundle(&first, None)?;
    let store = Arc::new(SweepStore::default());
    store.accept(&bundle)?;
    let topology = first.manifest().topology();
    let session = SessionRef::new(topology.session_id, topology.branch_id);
    let head = BranchHead::from_store(
        session,
        BranchRevision::from_store(1),
        CheckpointObjectRef::from_store(topology.checkpoint_id, bundle.root(), first.integrity()),
    );
    let lease = BranchLease::from_store(
        session,
        RunId::try_from(uuid::Uuid::from_u128(99))?,
        NonZeroU64::new(1).ok_or("nonzero fence")?,
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 1_000, 0)?,
        NonZeroU64::new(1_000).ok_or("nonzero interval")?,
    );
    let mut owner = PreparedCheckpoint::new(
        first,
        bundle,
        Some(DurableBranchFacts::new(
            store.clone(),
            head,
            lease,
            StoreTimeouts::default(),
        )),
    )
    .start();
    *lock_mutex(&store.authority) = Arc::downgrade(&owner.state);
    for cycle in 0..4_u128 {
        let tail = if cycle % 2 == 0 { 2_000 } else { 1_000 };
        let next = checkpoint(cycle + 2, tail)?;
        let prior = lock_mutex(&owner.state).bundle.clone();
        let publication = RuntimeCheckpointCodec::advance(&next, &prior)?;
        // This is an acknowledged publication into the test CAS.
        store.accept(&publication)?;
        let cached = publication.trimmed()?;
        let cold = RuntimeCheckpointCodec::bundle(&next, None)?;
        assert!(
            cached.stats().object_count() > cold.stats().object_count(),
            "ordinary trimming must leave obsolete membership in this regression"
        );
        install(&owner, next, cached);
        *lock_mutex(&store.lose_ack) = cycle % 2 == 0;
        let result = owner.prune_superseded(1).await;
        assert_eq!(result.is_err(), cycle % 2 == 0);
        let obsolete = history_segment(if tail == 2_000 { 1_000 } else { 2_000 })?;
        let obsolete_checkpoint =
            fixture_with_history(CanonicalHistory::new(vec![obsolete])?, 1, false)?;
        let obsolete_bundle = RuntimeCheckpointCodec::bundle(&obsolete_checkpoint, None)?;
        let address = obsolete_bundle
            .supplied_objects()
            .iter()
            .find(|object| object.address().kind() == RuntimeObjectKind::HistorySegment)
            .ok_or("history object")?
            .address();
        assert!(
            !lock_mutex(&store.objects).contains_key(&address),
            "old object was reclaimed"
        );
        // Reopen the exact surviving closure and continue on the same warm owner.
        let cache = lock_mutex(&owner.state).bundle.clone();
        let reopened = CheckpointObjectBundle::from_store(
            cache.checkpoint_id(),
            cache.checkpoint_integrity(),
            cache.root(),
            SweepStore::closure(&lock_mutex(&store.objects), cache.root())?
                .into_values()
                .collect(),
        )?;
        assert_eq!(
            reopened.validated_checkpoint()?.as_ref(),
            owner.checkpoint().as_ref()
        );
    }
    assert_eq!(*lock_mutex(&store.calls), 4);
    {
        let mut authority = lock_mutex(&owner.state);
        authority.bundle = without_cached_objects(&authority.bundle);
    }
    assert!(owner.prune_superseded(0).await.is_err());
    assert_eq!(
        *lock_mutex(&store.calls),
        4,
        "invalid cache refuses the sweep"
    );
    owner
        .heartbeat
        .stop_and_join(Duration::from_secs(1))
        .await?;
    Ok(())
}

/// Renewal answers scripted per call: `busy` leading renewals report
/// `BackendBusy`, and every renewal reports `LeaseExpired` once `expired`
/// is set, with revival answering `revive_error`.
struct FlakyRenewStore {
    colocated_full: std::sync::atomic::AtomicBool,
    now: std::sync::atomic::AtomicU64,
    busy: std::sync::atomic::AtomicUsize,
    expired: std::sync::atomic::AtomicBool,
    revive_once: std::sync::atomic::AtomicBool,
    revive_error: SessionStoreError,
    renewals: std::sync::atomic::AtomicUsize,
}

impl FlakyRenewStore {
    fn new(busy: usize, revive_error: SessionStoreError) -> Self {
        Self {
            colocated_full: std::sync::atomic::AtomicBool::new(false),
            now: std::sync::atomic::AtomicU64::new(0),
            busy: std::sync::atomic::AtomicUsize::new(busy),
            expired: std::sync::atomic::AtomicBool::new(false),
            revive_once: std::sync::atomic::AtomicBool::new(false),
            revive_error,
            renewals: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl SessionStore for FlakyRenewStore {
    fn recover_storage(&self) -> StoreFuture<'_, ()> {
        self.colocated_full
            .store(false, std::sync::atomic::Ordering::Release);
        Box::pin(async { Ok(()) })
    }

    fn storage_unavailable(&self) -> bool {
        self.colocated_full
            .load(std::sync::atomic::Ordering::Acquire)
    }
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        LeaseInstant::new(
            LeaseDomain::new([1; 16], [2; 32]),
            self.now.load(std::sync::atomic::Ordering::Acquire),
            0,
        )
    }

    plakous_test_support::session_store_methods! {
        unavailable store_api;
        create, recover_create, load_branch, load_checkpoint, acquire, release, commit,
        recover_commit, fork, recover_fork, list, archive, delete, recover_delete,
        feed_tail, branch_line, load_line, line_job_terminals, record_line_job_terminals,
        read_feed, usage, flush,
    }

    fn renew(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease> {
        use std::sync::atomic::Ordering;
        Box::pin(async move {
            self.renewals.fetch_add(1, Ordering::AcqRel);
            if self.expired.load(Ordering::Acquire) {
                return Err(SessionStoreError::LeaseExpired);
            }
            if self
                .busy
                .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(SessionStoreError::BackendBusy);
            }
            Ok(lease)
        })
    }

    fn revive(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease> {
        let error = self.revive_error;
        Box::pin(async move {
            use std::sync::atomic::Ordering;
            if !self.revive_once.swap(false, Ordering::AcqRel) {
                return Err(error);
            }
            self.expired.store(false, Ordering::Release);
            Ok(BranchLease::from_store(
                lease.session(),
                lease.owner(),
                lease.fence(),
                LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 103, 0)?,
                lease.lease_seconds(),
            ))
        })
    }

    fn shutdown(&self) -> StoreFuture<'_, DurabilityReport> {
        self.flush()
    }
}

/// Starts an owner holding a 3 s lease (renewed every second) that
/// expires at store second 100.
fn flaky_owner(store: Arc<FlakyRenewStore>) -> TestResult<CheckpointOwner> {
    let first = checkpoint(1, 1_000)?;
    let bundle = RuntimeCheckpointCodec::bundle(&first, None)?;
    let topology = first.manifest().topology();
    let session = SessionRef::new(topology.session_id, topology.branch_id);
    let head = BranchHead::from_store(
        session,
        BranchRevision::from_store(1),
        CheckpointObjectRef::from_store(topology.checkpoint_id, bundle.root(), first.integrity()),
    );
    let lease = BranchLease::from_store(
        session,
        RunId::try_from(uuid::Uuid::from_u128(99))?,
        NonZeroU64::new(1).ok_or("nonzero fence")?,
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 100, 0)?,
        NonZeroU64::new(3).ok_or("nonzero interval")?,
    );
    Ok(PreparedCheckpoint::new(
        first,
        bundle,
        Some(DurableBranchFacts::new(
            store,
            head,
            lease,
            StoreTimeouts::default(),
        )),
    )
    .start())
}

#[tokio::test(start_paused = true)]
async fn a_busy_renewal_is_retried_while_the_lease_is_live() -> TestResult {
    let store = Arc::new(FlakyRenewStore::new(2, SessionStoreError::LeaseHeld));
    let mut owner = flaky_owner(Arc::clone(&store))?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        store.renewals.load(std::sync::atomic::Ordering::Acquire) >= 3,
        "two busy answers and a successful retry"
    );
    owner.heartbeat.check()?;
    owner
        .heartbeat
        .stop_and_join(Duration::from_secs(1))
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_dispatch_revalidation_does_not_block_lease_renewal() -> TestResult {
    use std::sync::atomic::Ordering;

    let store = Arc::new(FlakyRenewStore::new(0, SessionStoreError::LeaseHeld));
    let owner = flaky_owner(Arc::clone(&store))?;
    let start = owner.start_authority(CancellationToken::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let dispatch = tokio::spawn({
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let first = std::sync::atomic::AtomicBool::new(true);
        async move {
            let _guard = start
                .admit_after(
                    || async {
                        if !first.swap(false, Ordering::AcqRel) {
                            return Ok(());
                        }
                        entered.notify_one();
                        release.notified().await;
                        Ok(())
                    },
                    std::convert::identity,
                )
                .await?;
            Ok::<_, AgentError>(())
        }
    });
    entered.notified().await;
    let renewal = tokio::time::timeout(Duration::from_secs(10), async {
        while store.renewals.load(Ordering::Acquire) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    release.notify_one();
    renewal?;
    dispatch.await??;
    owner.heartbeat.check()?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn remote_dispatch_rechecks_after_contention_storage_recovery_and_revival() -> TestResult {
    use std::sync::atomic::Ordering;
    let store = Arc::new(FlakyRenewStore::new(0, SessionStoreError::LeaseHeld));
    let owner = flaky_owner(Arc::clone(&store))?;
    let start = owner.start_authority(CancellationToken::new());
    let gate = lock_mutex(&owner.state)
        .durable
        .as_ref()
        .ok_or("durable branch")?
        .lease_gate
        .clone();
    let probes = std::sync::atomic::AtomicUsize::new(0);
    let _guard = start
        .admit_after(
            || async {
                match probes.fetch_add(1, Ordering::AcqRel) {
                    0 => {
                        let intervening = gate.clone().try_lock_owned().map_err(|_| {
                            AgentError::new(AgentErrorKind::Internal, "probe held the lease gate")
                        })?;
                        tokio::spawn(async move {
                            tokio::task::yield_now().await;
                            drop(intervening);
                        });
                    }
                    1 => store.colocated_full.store(true, Ordering::Release),
                    2 => {
                        store.now.store(100, Ordering::Release);
                        store.expired.store(true, Ordering::Release);
                        store.revive_once.store(true, Ordering::Release);
                    }
                    3 => {}
                    _ => panic!("unexpected additional check"),
                }
                Ok(())
            },
            std::convert::identity,
        )
        .await?;
    assert_eq!(probes.load(Ordering::Acquire), 4);
    assert!(!store.storage_unavailable());
    assert!(!store.expired.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn colocated_capacity_debt_never_extends_a_busy_runtime_lease() -> TestResult {
    let store = Arc::new(FlakyRenewStore::new(
        usize::MAX,
        SessionStoreError::LeaseHeld,
    ));
    store
        .colocated_full
        .store(true, std::sync::atomic::Ordering::Release);
    let mut owner = flaky_owner(store.clone())?;
    store.now.store(101, std::sync::atomic::Ordering::Release);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(owner.heartbeat.lost().is_cancelled());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(owner.heartbeat.check().is_err());
    let _ = owner.heartbeat.stop_and_join(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_revival_the_store_cannot_answer_is_not_reported_as_takeover() -> TestResult {
    for (revive_error, takeover) in [
        (SessionStoreError::BackendBusy, false),
        (SessionStoreError::BackendUnavailable, false),
        (SessionStoreError::LeaseHeld, true),
    ] {
        let store = Arc::new(FlakyRenewStore::new(0, revive_error));
        store
            .expired
            .store(true, std::sync::atomic::Ordering::Release);
        store.now.store(200, std::sync::atomic::Ordering::Release);
        let mut owner = flaky_owner(Arc::clone(&store))?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let failure = owner.heartbeat.failure();
        assert!(owner.heartbeat.lost().is_cancelled());
        assert_eq!(failure == lease_taken_over(), takeover, "{revive_error:?}");
        assert!(failure.is_durable_lease_loss(), "{revive_error:?}");
        let _ = owner.heartbeat.stop_and_join(Duration::from_secs(1)).await;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_expired_grants_corrupt_or_invalid_revival_is_not_proven_ownership_loss() -> TestResult {
    for cause in [
        SessionStoreError::corrupt(),
        SessionStoreError::UnsupportedSchema,
        SessionStoreError::Invalid(internal::StoreInvalidReason::LeaseExpiryOverflow),
        SessionStoreError::LeaseDomainMismatch,
    ] {
        let store = Arc::new(FlakyRenewStore::new(0, cause));
        store
            .expired
            .store(true, std::sync::atomic::Ordering::Release);
        store.now.store(200, std::sync::atomic::Ordering::Release);
        let mut owner = flaky_owner(store)?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(owner.heartbeat.lost().is_cancelled());
        let failure = owner.heartbeat.failure();
        assert_eq!(failure.store_error(), Some(cause));
        assert!(!failure.is_durable_lease_loss());
        let _ = owner.heartbeat.stop_and_join(Duration::from_secs(1)).await;
    }
    Ok(())
}

/// A store whose head moved on: every commit and prune is refused as
/// `NotLineHead`, while the lease itself still renews.
struct SupersededStore {
    commits: std::sync::atomic::AtomicUsize,
    prunes: std::sync::atomic::AtomicUsize,
}

impl SessionStore for SupersededStore {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 0, 0)
    }

    plakous_test_support::session_store_methods! {
        unavailable store_api;
        create, recover_create, load_branch, load_checkpoint, acquire, release, fork,
        recover_fork, list, archive, delete, recover_delete, feed_tail, branch_line,
        load_line, line_job_terminals, record_line_job_terminals, read_feed, usage, flush,
    }

    fn renew(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease> {
        Box::pin(async move { Ok(lease) })
    }

    fn commit(&self, _: CommitCheckpoint) -> StoreFuture<'_, CommitReceipt> {
        self.commits
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Box::pin(async { Err(SessionStoreError::NotLineHead) })
    }

    fn recover_commit(&self, _: CheckpointId) -> StoreFuture<'_, CommittedCheckpoint> {
        Box::pin(async { Err(SessionStoreError::NotFound) })
    }

    fn prune(&self, _: PruneBranch) -> StoreFuture<'_, PruneReceipt> {
        self.prunes
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Box::pin(async { Err(SessionStoreError::NotLineHead) })
    }

    fn shutdown(&self) -> StoreFuture<'_, DurabilityReport> {
        self.flush()
    }
}

fn superseded_owner(store: Arc<SupersededStore>) -> TestResult<CheckpointOwner> {
    let first = checkpoint(1, 1_000)?;
    let bundle = RuntimeCheckpointCodec::bundle(&first, None)?;
    let topology = first.manifest().topology();
    let session = SessionRef::new(topology.session_id, topology.branch_id);
    let head = BranchHead::from_store(
        session,
        BranchRevision::from_store(topology.branch_revision),
        CheckpointObjectRef::from_store(topology.checkpoint_id, bundle.root(), first.integrity()),
    );
    let lease = BranchLease::from_store(
        session,
        RunId::try_from(uuid::Uuid::from_u128(99))?,
        NonZeroU64::new(1).ok_or("nonzero fence")?,
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 1_000, 0)?,
        NonZeroU64::new(1_000).ok_or("nonzero interval")?,
    );
    Ok(PreparedCheckpoint::new(
        first,
        bundle,
        Some(DurableBranchFacts::new(
            store,
            head,
            lease,
            StoreTimeouts::default(),
        )),
    )
    .start())
}

/// A commit the head fence refuses fences the owner itself: nothing it
/// would publish later reaches the store.
#[tokio::test]
async fn a_commit_refused_as_not_line_head_fences_the_owner() -> TestResult {
    let store = Arc::new(SupersededStore {
        commits: std::sync::atomic::AtomicUsize::new(0),
        prunes: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut owner = superseded_owner(Arc::clone(&store))?;
    let prior = owner.checkpoint();
    let manifest = prior.manifest();
    let facts = || -> TestResult<SuccessorFacts> {
        Ok(SuccessorFacts {
            checkpoint_id: CheckpointId::try_from(uuid::Uuid::from_u128(0x5e_0001))?,
            lifecycle: RuntimeLifecycle::Quiescent,
            history: manifest.canonical().history().clone(),
            ledger: manifest.ledger().clone(),
            jobs: manifest.jobs().clone(),
            named_state: manifest.named_state().clone(),
            context: manifest.canonical().context().retained_for_exact_resume(),
            family: manifest.family_artifact().cloned(),
        })
    };
    let telemetry = DurabilityCommitTelemetry {
        emitter: Arc::new(TelemetryEmitter::disabled()),
        clock: Arc::new(SystemClock),
        context: TelemetryContext::new(
            DriverInstanceId::try_from(uuid::Uuid::from_u128(0x5e_0002))?,
            manifest.topology().session_id,
            manifest.topology().branch_id,
            RunId::try_from(uuid::Uuid::from_u128(0x5e_0003))?,
        ),
    };
    let refused = owner
        .publish(facts()?, None, &telemetry, StoreAdmission::Ordinary)
        .await
        .err()
        .ok_or("a superseded commit published")?;
    assert_eq!(refused.store_error(), Some(SessionStoreError::NotLineHead));
    assert_eq!(
        owner
            .heartbeat
            .check()
            .err()
            .and_then(|error| error.store_error()),
        Some(SessionStoreError::NotLineHead)
    );
    assert!(
        owner
            .publish(facts()?, None, &telemetry, StoreAdmission::Ordinary)
            .await
            .is_err()
    );
    assert_eq!(
        store.commits.load(std::sync::atomic::Ordering::Acquire),
        1,
        "a fenced owner never commits again"
    );
    let _ = owner.heartbeat.stop_and_join(Duration::from_secs(1)).await;
    Ok(())
}

/// A prune the head fence refuses fences the owner the same way.
#[tokio::test]
async fn a_prune_refused_as_not_line_head_fences_the_owner() -> TestResult {
    let store = Arc::new(SupersededStore {
        commits: std::sync::atomic::AtomicUsize::new(0),
        prunes: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut owner = superseded_owner(Arc::clone(&store))?;
    let refused = owner
        .prune_superseded(0)
        .await
        .err()
        .ok_or("a superseded prune succeeded")?;
    assert_eq!(refused.store_error(), Some(SessionStoreError::NotLineHead));
    assert!(owner.heartbeat.check().is_err());
    assert!(owner.prune_superseded(0).await.is_err());
    assert_eq!(store.prunes.load(std::sync::atomic::Ordering::Acquire), 1);
    let _ = owner.heartbeat.stop_and_join(Duration::from_secs(1)).await;
    Ok(())
}
