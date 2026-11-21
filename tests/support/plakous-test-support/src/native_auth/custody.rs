use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use plakous_auth::custody::local::{self, LocalCustody};
use plakous_auth::custody::{
    CustodyError, CustodySlot, CustodyStatus, ExpectedFence, IdentityDigest,
};
use plakous_auth::{CredentialRecord, CredentialStatus, PrivateCredentialStore, SecretBytes};

use super::TestResult;

/// A fields read must use the exact Ready record and keep its lease alive.
///
/// # Errors
/// Returns any fixture credential-read or fence-construction error.
///
/// # Panics
/// Panics if the credential fields or their lease do not match the fence.
pub async fn fields_hold_exact_fence(
    manager: &impl LocalCustody,
    slot: &CustodySlot,
    headers: &[(&str, &[u8])],
) -> TestResult {
    let fence = local::facts(manager, slot)
        .await?
        .expected_fence()
        .ok_or("no fence")?;
    let mut fields = local::fields(manager, slot, &fence).await?;
    assert!(fields.take_hold().is_some(), "the Ready lease is held");
    assert!(fields.names().eq(headers.iter().map(|(name, _)| *name)));
    for (name, value) in headers {
        assert_eq!(
            fields
                .take(name)
                .ok_or("missing field")?
                .into_header_bytes(),
            *value
        );
    }
    let moved = ExpectedFence {
        generation: fence.generation.next()?,
        ..fence
    };
    assert!(matches!(
        local::fields(manager, slot, &moved).await,
        Err(CustodyError::FenceMoved { .. })
    ));
    let foreign = ExpectedFence {
        account_binding: IdentityDigest::from_bytes([7; 32]),
        ..fence
    };
    assert!(matches!(
        local::fields(manager, slot, &foreign).await,
        Err(CustodyError::AccountChanged)
    ));
    Ok(())
}

/// A refresh committed between the two reads returns the moved fence; a
/// newly prepared read under that fence succeeds.
///
/// # Errors
/// Returns a fixture error or the unexpected outcome of the stale read.
///
/// # Panics
/// Panics if the stale request does not name the new Ready fence.
pub async fn fields_name_the_moved_fence(
    manager: &impl LocalCustody,
    slot: &CustodySlot,
    path: &Path,
) -> TestResult {
    let expected = local::facts(manager, slot)
        .await?
        .expected_fence()
        .ok_or("no fence")?;
    install_before_lease(path, LeaseRead::Fields, Box::new(advance_ready_unchanged));
    let refusal = local::fields(manager, slot, &expected).await;
    let Err(CustodyError::FenceMoved { facts }) = refusal else {
        return Err(format!("the moved record was not named: {refusal:?}").into());
    };
    assert_eq!(facts.generation, Some(expected.generation.next()?));
    assert_eq!(facts.account_binding, Some(expected.account_binding));
    let moved = facts.expected_fence().ok_or("no moved fence")?;
    assert!(
        local::fields(manager, slot, &moved)
            .await?
            .take_hold()
            .is_some()
    );
    Ok(())
}

/// Repeatedly move the Ready fence before the snapshot can lease its payload.
///
/// # Errors
/// Returns a fixture read or generation error.
///
/// # Panics
/// Panics if bounded retries report a storage fault or expose a stale snapshot.
pub async fn moving_snapshots_are_busy(
    manager: &impl LocalCustody,
    slot: &CustodySlot,
    store: &PrivateCredentialStore,
) -> TestResult {
    let before = local::facts(manager, slot)
        .await?
        .generation
        .ok_or("no generation")?;
    for _ in 0..3 {
        install_before_lease(
            store.path(),
            LeaseRead::Snapshot,
            Box::new(advance_ready_unchanged),
        );
    }
    assert!(matches!(
        local::facts(manager, slot).await,
        Err(CustodyError::Busy {
            retry_after_ms: 100
        })
    ));
    assert_eq!(
        local::facts(manager, slot).await?.generation,
        Some(before.next()?.next()?.next()?)
    );
    Ok(())
}

/// A rejection naming another login selection must not claim the current record.
///
/// # Errors
/// Returns a fixture read error.
///
/// # Panics
/// Panics if the refusal does not name the current, unchanged fence.
pub async fn rejection_names_current_selection(
    manager: &impl LocalCustody,
    slot: &CustodySlot,
) -> TestResult {
    let before = local::facts(manager, slot).await?;
    let fence = before.expected_fence().ok_or("no fence")?;
    let mut other = *before
        .selection_epoch_digest
        .ok_or("no selection")?
        .as_bytes();
    other[0] ^= 1;
    let refusal = local::after_rejection(
        manager,
        slot,
        &fence,
        Some(IdentityDigest::from_bytes(other)),
    )
    .await;
    assert_eq!(
        refusal,
        Err(CustodyError::FenceMoved {
            facts: Box::new(before.clone())
        })
    );
    assert_eq!(local::facts(manager, slot).await?, before);
    Ok(())
}

/// A Ready record cannot deliver fields under an inactive profile.
///
/// # Errors
/// Returns a fixture read error.
///
/// # Panics
/// Panics if profile inactivity does not block a lease.
pub async fn inactive_profile_refuses_fields(
    manager: &impl LocalCustody,
    slot: &CustodySlot,
) -> TestResult {
    let before = local::facts(manager, slot).await?;
    let fence = before.expected_fence().ok_or("no fence")?;
    assert!(matches!(
        local::fields(manager, slot, &fence).await,
        Err(CustodyError::NotReady { facts }) if *facts == before
    ));
    Ok(())
}

/// An outstanding peer claim is observed as Refreshing after the bounded
/// settle wait, without making a second claim. Call on unpaused current-thread Tokio.
///
/// # Errors
/// Returns a fixture read, claim, or restoration error.
///
/// # Panics
/// Panics if the reader skips its wait, exceeds the bound, or moves the claim.
pub async fn rejection_waits_for_peer_refresh(
    manager: &impl LocalCustody,
    slot: &CustodySlot,
    store: &PrivateCredentialStore,
) -> TestResult {
    let before = local::facts(manager, slot).await?;
    let rejected = before.expected_fence().ok_or("no fence")?;
    let claim = store
        .on_disk(|store| {
            let fence = store
                .load()?
                .ok_or(plakous_auth::PrivateStoreError::RecordMissing)?;
            store.begin_refresh(&fence)
        })
        .await?;
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    let answer =
        local::after_rejection(manager, slot, &rejected, before.selection_epoch_digest).await?;
    assert!((Duration::from_secs(30)..Duration::from_secs(31)).contains(&start.elapsed()));
    assert_eq!(answer.status, CustodyStatus::Refreshing);
    assert_eq!(answer.generation, Some(claim.fence().generation()));
    let restored = store
        .on_disk(move |store| store.restore_refresh_definitely_not_started(claim))
        .await?;
    let after = local::facts(manager, slot).await?;
    assert_eq!(after.status, CustodyStatus::Ready);
    assert_eq!(after.generation, Some(restored.generation()));
    Ok(())
}

/// A Ready lease held for an exchange blocks the claim without moving the
/// fence. Call on an unpaused current-thread Tokio runtime.
///
/// # Errors
/// Returns a fixture read error or failure to set up the exact lease.
///
/// # Panics
/// Panics if the runtime cannot pause time or contention changes the fence.
pub async fn rejected_while_leased_is_busy(
    manager: &impl LocalCustody,
    slot: &CustodySlot,
) -> TestResult {
    let facts = local::facts(manager, slot).await?;
    let rejected = facts.expected_fence().ok_or("no fence")?;
    let fields = local::fields(manager, slot, &rejected).await?;
    tokio::time::pause();
    let result =
        local::after_rejection(manager, slot, &rejected, facts.selection_epoch_digest).await;
    assert!(matches!(
        result,
        Err(CustodyError::Busy {
            retry_after_ms: 100
        })
    ));
    assert_eq!(
        local::facts(manager, slot).await?.expected_fence(),
        Some(rejected)
    );
    drop(fields);
    Ok(())
}

/// Simultaneous rejections adopt one replacement of the same login. The
/// dialect fixture separately verifies the exact exchanges at its TLS peer.
///
/// # Errors
/// Returns a fixture read, task join, or refresh error.
///
/// # Panics
/// Panics if the record moves for a foreign rejection or the concurrent
/// rejections do not converge on one replacement of the original login.
pub async fn concurrent_rejections_adopt_one_replacement<D: LocalCustody + Send + 'static>(
    observer: &D,
    refresher: &Arc<D>,
    slot: &CustodySlot,
) -> TestResult {
    let facts = local::facts(observer, slot).await?;
    let rejected = facts.expected_fence().ok_or("no fence")?;
    let selection = facts.selection_epoch_digest;
    let foreign = ExpectedFence {
        account_binding: IdentityDigest::from_bytes([7; 32]),
        ..rejected
    };
    assert_eq!(
        local::after_rejection(refresher.as_ref(), slot, &foreign, selection)
            .await?
            .expected_fence(),
        Some(rejected),
        "another account's rejection cannot claim this record"
    );

    let mut callers = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let (refresher, slot) = (Arc::clone(refresher), slot.clone());
        callers.spawn(async move {
            local::after_rejection(refresher.as_ref(), &slot, &rejected, selection).await
        });
    }
    let generation = Some(rejected.generation.next()?.next()?);
    let mut count = 0;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(refresh) = callers.join_next().await {
            let answer = refresh??;
            assert_eq!(answer.status, CustodyStatus::Ready);
            assert_eq!(answer.generation, generation);
            assert_eq!(answer.account_binding, facts.account_binding);
            assert_eq!(answer.selection_epoch_digest, selection);
            count += 1;
        }
        Ok::<_, super::TestError>(())
    })
    .await
    .map_err(|_| format!("custody refresh join stalled: {count}/4 returned"))??;
    let current = local::facts(observer, slot).await?;
    assert_eq!(current.status, CustodyStatus::Ready);
    assert_eq!(current.generation, generation);
    assert_eq!(current.account_binding, facts.account_binding);
    assert_eq!(current.selection_epoch_digest, selection);
    Ok(())
}

/// The custody read whose lease a fixture can race.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum LeaseRead {
    /// The payload read that projects the snapshot's facts.
    Snapshot,
    /// The exact Ready lease that keeps the returned fields live.
    Fields,
}

type Hook = Box<dyn FnOnce(&PrivateCredentialStore) + Send>;
type InstalledHook = (PathBuf, LeaseRead, Hook);
static BEFORE_LEASE: Mutex<Vec<InstalledHook>> = Mutex::new(Vec::new());

/// Installs a one-shot action before the next matching lease on this store.
pub fn install_before_lease(path: &Path, read: LeaseRead, hook: Hook) {
    BEFORE_LEASE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((path.to_owned(), read, hook));
}

/// Runs the one-shot action, outside the hook registry lock.
pub fn run_before_lease(store: &PrivateCredentialStore, read: LeaseRead) {
    let hook = {
        let mut hooks = BEFORE_LEASE.lock().unwrap_or_else(PoisonError::into_inner);
        hooks
            .iter()
            .position(|(path, point, _)| path == store.path() && *point == read)
            .map(|index| hooks.remove(index).2)
    };
    if let Some(hook) = hook {
        hook(store);
    }
}

/// Simulates a peer's completed refresh without changing the payload.
///
/// # Panics
/// Panics if the fixture does not have a writable Ready record.
#[allow(clippy::expect_used, reason = "a fixture's peer store write")]
pub fn advance_ready_unchanged(store: &PrivateCredentialStore) {
    let fence = store.load().ok().flatten().expect("a stored record");
    let payload = {
        let lease = store.acquire_ready(&fence).expect("a Ready record");
        SecretBytes::new(lease.payload().expose_secret().to_vec())
    };
    let record = CredentialRecord::new(
        fence.profile_id().clone(),
        fence.account_binding().clone(),
        fence.generation().next().expect("a next generation"),
        CredentialStatus::Ready,
        payload,
    )
    .expect("a record");
    store
        .replace(fence.generation(), &record)
        .expect("the refresh commits");
}
