use super::*;
use crate::integrity_fixture as fixture;

struct FixedClock;

impl LeaseClock for FixedClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 100, 0)
    }
}

fn replay_fixture() -> fixture::TestResult<(MemorySessionStore, CreateSession)> {
    let store = MemorySessionStore::with_clock(MemoryStoreLimits::default(), Arc::new(FixedClock))?;
    let selector = SessionRef::new(fixture::id(1)?, fixture::id(2)?);
    let initial = fixture::checkpoint(
        fixture::topology(
            selector.session_id(),
            selector.branch_id(),
            fixture::id(100)?,
            None,
            0,
            None,
            1,
        )?,
        1,
        RuntimeDurability::Disabled,
    )?;
    let request = CreateSession::new(
        selector,
        plakous_agent::internal::Timestamp::new(50, 0)?,
        fixture::id(3)?,
        NonZeroU64::new(1_000).ok_or("nonzero lease")?,
        fixture::bundle(&initial, None)?,
    );
    Ok((store, request))
}

#[test]
fn memory_create_replay_and_recovery_require_checkpoint_commit_identity() -> fixture::TestResult {
    let (store, request) = replay_fixture()?;
    let receipt = store.create_sync(&request)?;
    assert_eq!(store.create_sync(&request)?, receipt);
    assert_eq!(store.recover_create_sync(&request)?, receipt);
    let operation = receipt.operation();
    let wrong_identity = plakous_agent::internal::Digest::sha256(
        DigestDomain::ProviderOperation,
        1,
        &[b"different commit"],
    )?;
    // Integrity already binds commit identity. Damage only the retained receipt
    // to exercise the same defense-in-depth replay predicate as SQLite.
    store.lock()?.creates.insert(
        request.session().session_id(),
        CreateReceipt::from_store(
            CreateOperation::from_store(
                operation.session(),
                operation.created_at(),
                operation.owner(),
                operation.lease_seconds(),
                operation.initial(),
                wrong_identity,
            ),
            receipt.session(),
            receipt.branch(),
            receipt.lease().clone(),
            receipt.feed_seq(),
        ),
    );
    let head = store.load_branch_sync(request.session())?.head();
    let tail = store.feed_tail_sync(request.session().session_id())?;
    assert_eq!(
        store.create_sync(&request),
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        store.recover_create_sync(&request),
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(store.load_branch_sync(request.session())?.head(), head);
    assert_eq!(store.feed_tail_sync(request.session().session_id())?, tail);
    Ok(())
}

#[test]
fn memory_commit_replay_requires_checkpoint_commit_identity() -> fixture::TestResult {
    let (store, create) = replay_fixture()?;
    let created = store.create_sync(&create)?;
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            create.session().session_id(),
            create.session().branch_id(),
            fixture::id(101)?,
            Some(create.initial().checkpoint_id()),
            1,
            None,
            1,
        )?,
        2,
        RuntimeDurability::Disabled,
    )?;
    let request = CommitCheckpoint::new(
        created.lease().clone(),
        created.branch(),
        fixture::bundle(&checkpoint, Some(create.initial()))?,
    );
    let receipt = store.commit_sync(&request)?;
    assert_eq!(store.commit_sync(&request)?, receipt);
    let committed = store.recover_commit_sync(receipt.checkpoint().checkpoint_id())?;
    let operation = committed.operation();
    let wrong_identity = plakous_agent::internal::Digest::sha256(
        DigestDomain::ProviderOperation,
        1,
        &[b"different commit"],
    )?;
    store.lock()?.commits.insert(
        receipt.checkpoint().checkpoint_id(),
        CommittedCheckpoint::from_store(
            CommitOperation::from_store(
                operation.lease().clone(),
                operation.expected(),
                operation.checkpoint(),
                wrong_identity,
            ),
            receipt,
        ),
    );
    let tail = store.feed_tail_sync(create.session().session_id())?;
    assert_eq!(
        store.commit_sync(&request),
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        store.load_branch_sync(create.session())?.head(),
        receipt.head()
    );
    assert_eq!(store.feed_tail_sync(create.session().session_id())?, tail);
    Ok(())
}

fn prune_fixture() -> fixture::TestResult<(MemorySessionStore, PruneBranch)> {
    let store = MemorySessionStore::with_clock(MemoryStoreLimits::default(), Arc::new(FixedClock))?;
    let selector = SessionRef::new(fixture::id(1)?, fixture::id(2)?);
    let mut prior = fixture::bundle(
        &fixture::checkpoint(
            fixture::topology(
                selector.session_id(),
                selector.branch_id(),
                fixture::id(100)?,
                None,
                0,
                None,
                1,
            )?,
            1,
            RuntimeDurability::Disabled,
        )?,
        None,
    )?;
    let created = store.create_sync(&CreateSession::new(
        selector,
        plakous_agent::internal::Timestamp::new(50, 0)?,
        fixture::id(3)?,
        NonZeroU64::new(1_000).ok_or("nonzero lease")?,
        prior.clone(),
    ))?;
    let mut head = created.branch();
    for revision in 1..=4 {
        let next = fixture::checkpoint(
            fixture::topology(
                selector.session_id(),
                selector.branch_id(),
                fixture::id(100 + u128::from(revision))?,
                Some(prior.checkpoint_id()),
                revision,
                None,
                1,
            )?,
            2,
            RuntimeDurability::Disabled,
        )?;
        prior = fixture::bundle(&next, Some(&prior))?;
        head = store
            .commit_sync(&CommitCheckpoint::new(
                created.lease().clone(),
                head,
                prior.clone(),
            ))?
            .head();
    }
    Ok((
        store,
        PruneBranch::new(created.lease().clone(), head, BTreeSet::new(), 1),
    ))
}

#[test]
fn memory_prune_preserves_retained_map_allocation() -> fixture::TestResult {
    let (store, request) = prune_fixture()?;
    let session = request.expected().session().session_id();
    let before = std::ptr::from_ref(store.lock()?.sessions.get(&session).ok_or("session")?);
    let receipt = store.prune_sync(&request)?;
    assert_eq!(receipt.pruned_checkpoints(), 2);
    let state = store.lock()?;
    assert_eq!(
        before,
        std::ptr::from_ref(state.sessions.get(&session).ok_or("session")?)
    );
    assert!(!state.checkpoints.contains_key(&fixture::id(101)?));
    assert!(state.checkpoints.contains_key(&fixture::id(104)?));
    Ok(())
}

#[test]
fn memory_prune_failed_sweep_leaves_state_unchanged() -> fixture::TestResult {
    for corrupt_graph in [true, false] {
        let (store, request) = prune_fixture()?;
        {
            let mut state = store.lock()?;
            if corrupt_graph {
                state
                    .objects
                    .remove(&request.expected().checkpoint().root());
            } else {
                state.object_bytes = 0;
            }
        }
        let before = store.lock()?.clone();
        assert_eq!(
            store.prune_sync(&request),
            Err(SessionStoreError::corrupt())
        );
        let after = store.lock()?;
        assert_eq!(
            after.checkpoints.keys().collect::<Vec<_>>(),
            before.checkpoints.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            after.commits.keys().collect::<Vec<_>>(),
            before.commits.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            after.hot_bundles.keys().collect::<Vec<_>>(),
            before.hot_bundles.keys().collect::<Vec<_>>()
        );
        assert_eq!(after.objects, before.objects);
        assert_eq!(after.object_bytes, before.object_bytes);
        assert_eq!(after.pruned, before.pruned);
        assert_eq!(after.horizons, before.horizons);
    }
    Ok(())
}

#[test]
fn missing_publication_base_or_root_leaves_memory_state_unchanged() -> fixture::TestResult {
    use plakous_agent::internal::StoreIntegrityReason;
    for reason in [
        StoreIntegrityReason::MissingBase,
        StoreIntegrityReason::MissingRoot,
    ] {
        let (base, next) = fixture::integrity_publication_pair()?;
        let mut state = StoreState::default();
        install_candidate(
            &mut state,
            publication_candidate(&base)?,
            MemoryStoreLimits::default(),
            StoreAdmission::Ordinary,
        )?;
        let before = (
            state.checkpoints.len(),
            state.objects.len(),
            state.object_bytes,
        );
        let mut candidate = publication_candidate(&next)?;
        if reason == StoreIntegrityReason::MissingBase {
            candidate.base_root = Some(candidate.reference.root());
        } else {
            candidate
                .objects
                .retain(|object| object.address() != candidate.reference.root());
        }
        assert_eq!(
            install_candidate(
                &mut state,
                candidate,
                MemoryStoreLimits::default(),
                StoreAdmission::Ordinary
            )
            .err(),
            Some(SessionStoreError::Corrupt(reason)),
        );
        assert_eq!(
            (
                state.checkpoints.len(),
                state.objects.len(),
                state.object_bytes
            ),
            before
        );
        assert_eq!(
            state
                .checkpoints
                .get(&base.checkpoint_id())
                .ok_or("base checkpoint")?
                .reference,
            bundle_reference(&base)
        );
        assert!(!state.checkpoints.contains_key(&next.checkpoint_id()));
    }
    Ok(())
}
