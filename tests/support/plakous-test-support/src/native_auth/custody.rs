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
pub async fn fields_name_the_moved_fence(
    manager: &impl LocalCustody,
    slot: &CustodySlot,
    advance_between_reads: impl FnOnce() -> TestResult,
) -> TestResult {
    let expected = local::facts(manager, slot)
        .await?
        .expected_fence()
        .ok_or("no fence")?;
    advance_between_reads()?;
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

/// A Ready lease held for an exchange blocks the claim without moving the
/// fence. Call on an unpaused current-thread Tokio runtime.
///
/// # Errors
/// Returns a fixture read error or failure to set up the exact lease.
///
/// # Panics
/// Panics if the runtime cannot pause time.
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

    let mut refreshes = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let (refresher, slot) = (Arc::clone(refresher), slot.clone());
        refreshes.spawn(async move {
            local::after_rejection(refresher.as_ref(), &slot, &rejected, selection).await
        });
    }
    let generation = Some(rejected.generation.next()?.next()?);
    let mut count = 0;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(refresh) = refreshes.join_next().await {
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

type Hook = Box<dyn FnOnce() + Send>;
static BEFORE_LEASE: Mutex<Vec<(String, Hook)>> = Mutex::new(Vec::new());

/// Installs a one-shot action before the next fields lease for this account.
pub fn install_before_lease(account: &str, hook: Hook) {
    BEFORE_LEASE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((account.to_owned(), hook));
}

/// Runs the one-shot action, outside the hook registry lock.
pub fn run_before_lease(account: &str) {
    let hook = {
        let mut hooks = BEFORE_LEASE.lock().unwrap_or_else(PoisonError::into_inner);
        hooks
            .iter()
            .position(|(owner, _)| owner == account)
            .map(|index| hooks.remove(index).1)
    };
    if let Some(hook) = hook {
        hook();
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
