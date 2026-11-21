//! The local custody loops around a dialect's snapshot and claimed refresh.

use std::future::Future;
use std::time::Duration;

use super::{
    AccountFacts, CredentialFields, CredentialHold, CustodyError, CustodySlot, CustodyStatus,
    ExpectedFence, IdentityDigest,
};
use crate::{CredentialFence, PrivateCredentialStore, PrivateStoreError, ReadyCredentialLease};

const SNAPSHOT_ATTEMPTS: usize = 3;
const FENCE_READS: usize = 2;
const SETTLE_WAIT: Duration = Duration::from_secs(30);
const SETTLE_POLL: Duration = Duration::from_millis(100);

/// Facts and their local fence.
pub type Snapshot = (AccountFacts, Option<CredentialFence>);

/// A dialect's store for exact Ready field leases. The handle is not exposed
/// to callers of the dialect.
pub struct LeaseSource<'a>(&'a PrivateCredentialStore);

impl<'a> LeaseSource<'a> {
    #[must_use]
    pub const fn new(store: &'a PrivateCredentialStore) -> Self {
        Self(store)
    }
}

/// Local dialect hooks. Store and claim operations in these hooks must run
/// off the async executor. The dialect owns lifecycle/format refusal, account
/// and selection digests, field parsing, and the claimed grant itself.
pub trait LocalCustody: Sync {
    fn snapshot_once<'a>(
        &'a self,
        slot: &'a CustodySlot,
    ) -> impl Future<Output = Result<Snapshot, CustodyError>> + Send + 'a;

    fn profile_active(&self) -> bool;

    fn lease_source(&self) -> Option<LeaseSource<'_>>;

    /// Fixture seam, called on the blocking worker before an exact fields lease.
    fn before_fields_lease(_store: &PrivateCredentialStore) {}

    /// Decode only from the lease; the common flow attaches it to the fields.
    fn fields_from_lease(lease: &ReadyCredentialLease) -> Result<CredentialFields, CustodyError>;

    /// Run the claimed refresh pinned to this account and login selection.
    fn refresh_rejected<'a>(
        &'a self,
        facts: &'a AccountFacts,
        current: &'a CredentialFence,
        rejected: &'a ExpectedFence,
        selection: IdentityDigest,
    ) -> impl Future<Output = Result<(), CustodyError>> + Send + 'a;
}

async fn snapshot<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
) -> Result<Snapshot, CustodyError> {
    retry_snapshot(|| dialect.snapshot_once(slot)).await
}

/// Retry a moving snapshot. Also used to reread
/// after a once-only identity hydration without starting another hydration.
///
/// # Errors
/// Returns the first non-retryable or last retryable refusal.
pub async fn retry_snapshot<T, F: Future<Output = Result<T, CustodyError>>>(
    mut read: impl FnMut() -> F,
) -> Result<T, CustodyError> {
    let mut attempts = 1;
    loop {
        match read().await {
            Err(CustodyError::Busy { .. }) if attempts < SNAPSHOT_ATTEMPTS => attempts += 1,
            result => return result,
        }
    }
}

async fn settled_snapshot<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
) -> Result<Snapshot, CustodyError> {
    let deadline = tokio::time::Instant::now() + SETTLE_WAIT;
    loop {
        let current = snapshot(dialect, slot).await?;
        if current.0.status != CustodyStatus::Refreshing || tokio::time::Instant::now() >= deadline
        {
            return Ok(current);
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
}

/// Read non-secret custody facts.
///
/// # Errors
/// Returns the dialect's snapshot refusal.
pub async fn facts<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
) -> Result<AccountFacts, CustodyError> {
    Ok(snapshot(dialect, slot).await?.0)
}

/// Inject fields only from the exact Ready lease; recheck the current facts
/// once if the record moves between the facts read and the lease.
///
/// # Errors
/// Returns an exact fence, store, or profile refusal.
pub async fn fields<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
    expected: &ExpectedFence,
) -> Result<CredentialFields, CustodyError> {
    for read in 0..FENCE_READS {
        let (facts, current) = if read == 0 {
            snapshot(dialect, slot).await?
        } else {
            settled_snapshot(dialect, slot).await?
        };
        facts.check_fence(expected)?;
        if !dialect.profile_active() {
            return Err(CustodyError::NotReady {
                facts: Box::new(facts),
            });
        }
        let fence = current.ok_or(CustodyError::Internal)?;
        let source = dialect.lease_source().ok_or(CustodyError::Store)?;
        match source
            .0
            .on_disk(move |store| {
                D::before_fields_lease(store);
                store.acquire_ready(&fence)
            })
            .await
            .map_err(CustodyError::from)
        {
            Ok(lease) => {
                return Ok(D::fields_from_lease(&lease)?.holding(CredentialHold::new(lease)));
            }
            Err(CustodyError::Busy { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Err(PrivateStoreError::StoreBusy.into())
}

fn check_selection(
    facts: &AccountFacts,
    rejected: &ExpectedFence,
    selection: IdentityDigest,
) -> Result<(), CustodyError> {
    if facts.account_binding == Some(rejected.account_binding)
        && facts
            .selection_epoch_digest
            .is_some_and(|current| current != selection)
    {
        return Err(CustodyError::FenceMoved {
            facts: Box::new(facts.clone()),
        });
    }
    Ok(())
}

/// Settle a peer's claim before judging a rejection, and again after this
/// caller's attempt. A rejection never grants authority over a different account.
///
/// # Errors
/// Returns a snapshot refusal or the dialect's refusal to refresh.
pub async fn after_rejection<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
    rejected: &ExpectedFence,
    selection: Option<IdentityDigest>,
) -> Result<AccountFacts, CustodyError> {
    let (facts, current) = settled_snapshot(dialect, slot).await?;
    let (Some(current), Some(selection)) = (current, selection.or(facts.selection_epoch_digest))
    else {
        return Ok(facts);
    };
    if facts.account_binding != Some(rejected.account_binding) {
        return Ok(facts);
    }
    check_selection(&facts, rejected, selection)?;
    if facts.status == CustodyStatus::Refreshing {
        return Ok(facts);
    }
    dialect
        .refresh_rejected(&facts, &current, rejected, selection)
        .await?;
    let (current, _) = settled_snapshot(dialect, slot).await?;
    check_selection(&current, rejected, selection)?;
    Ok(current)
}
